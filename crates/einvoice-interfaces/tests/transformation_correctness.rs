//! Transformation correctness: does a transformed document say what its
//! source said?
//!
//! `tests/xsd_validation.rs` compares the hub read from a sample with the hub
//! read back from the emitted document. That comparison runs both halves of
//! the same spoke, so a reader and writer that agree on the *wrong* element
//! pass it. This harness instead reads source and output through an oracle
//! that knows nothing about the mappings: [`TERMS`], the EN 16931 business
//! terms with their element paths from the official syntax bindings
//! (EN 16931-3-2 UBL, EN 16931-3-3 CII, the AgID EN 16931 → FatturaPA
//! mapping). For every source and every target it checks:
//!
//! 1. **Fidelity** — every oracle term the source carries arrives in the
//!    output with the same value (numbers compared as decimals, dates as
//!    `YYYYMMDD`, codes case-insensitively). A term the target syntax cannot
//!    carry has no path there and is skipped.
//! 2. **Totals** — the output satisfies the EN 16931 calculation rules
//!    (BR-CO-10, -13, -14, -15, -16 and the per-rate VAT amount) wherever the
//!    source does.
//! 3. **Determinism** — the same input yields byte-identical output twice.
//! 4. **Idempotence** — re-transforming an output into its own format
//!    changes nothing.
//! 5. **Path independence** — source → A → B says nothing that contradicts
//!    source → B: a lossy hop may drop terms, never change or invent them.
//!
//! Sources are the samples the mappings declare (`[[meta.samples]]`), plus
//! every `*.xml` under `KRAB_CORPUS_DIR` when set (absolute, or relative to
//! the workspace root; format auto-detected), so
//! a third-party corpus such as the KoSIT XRechnung test suite can be run
//! without vendoring it. Transforms a target refuses (error diagnostics) are
//! skipped: which refusals are documented is `xsd_validation.rs`'s concern.
//! The full report shows with `--nocapture`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use einvoice_interfaces::cli::detect_source;
use einvoice_interfaces::{Engine, Spoke};
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// How a term's values are normalized before comparison.
#[derive(Clone, Copy)]
enum Kind {
    /// Trimmed, inner whitespace collapsed.
    Text,
    /// Trimmed, upper-cased.
    Code,
    /// `YYYY-MM-DD` (UBL, FatturaPA) and `YYYYMMDD` (CII format 102) alike.
    Date,
    /// A decimal, trailing zeros insignificant.
    Number,
}

/// One EN 16931 business term and its element path per syntax, local names
/// from the root, `@name` for an attribute. `""`: the syntax cannot carry it.
struct Term {
    bt: &'static str,
    kind: Kind,
    /// Whether document order is significant (lines); otherwise the values
    /// compare as a multiset (VAT breakdown, allowances).
    ordered: bool,
    ubl: &'static str,
    cii: &'static str,
    fpa: &'static str,
}

macro_rules! ubl {
    ($p:literal) => {
        concat!("/Invoice/", $p)
    };
}
macro_rules! cii_doc {
    ($p:literal) => {
        concat!("/CrossIndustryInvoice/ExchangedDocument/", $p)
    };
}
macro_rules! cii_agr {
    ($p:literal) => {
        concat!(
            "/CrossIndustryInvoice/SupplyChainTradeTransaction/ApplicableHeaderTradeAgreement/",
            $p
        )
    };
}
macro_rules! cii_set {
    ($p:literal) => {
        concat!(
            "/CrossIndustryInvoice/SupplyChainTradeTransaction/ApplicableHeaderTradeSettlement/",
            $p
        )
    };
}
macro_rules! cii_sum { ($p:literal) => { concat!("/CrossIndustryInvoice/SupplyChainTradeTransaction/ApplicableHeaderTradeSettlement/SpecifiedTradeSettlementHeaderMonetarySummation/", $p) }; }
macro_rules! cii_line {
    ($p:literal) => {
        concat!(
            "/CrossIndustryInvoice/SupplyChainTradeTransaction/IncludedSupplyChainTradeLineItem/",
            $p
        )
    };
}
macro_rules! fpa_hdr {
    ($p:literal) => {
        concat!("/FatturaElettronica/FatturaElettronicaHeader/", $p)
    };
}
macro_rules! fpa_doc {
    ($p:literal) => {
        concat!(
            "/FatturaElettronica/FatturaElettronicaBody/DatiGenerali/DatiGeneraliDocumento/",
            $p
        )
    };
}
macro_rules! fpa_goods {
    ($p:literal) => {
        concat!(
            "/FatturaElettronica/FatturaElettronicaBody/DatiBeniServizi/",
            $p
        )
    };
}

const fn term(
    bt: &'static str,
    kind: Kind,
    ordered: bool,
    ubl: &'static str,
    cii: &'static str,
    fpa: &'static str,
) -> Term {
    Term {
        bt,
        kind,
        ordered,
        ubl,
        cii,
        fpa,
    }
}

use Kind::{Code, Date, Number, Text};

// ponytail: the core terms every format here carries, not all ~160 BTs; add a
// row when a mapping starts carrying a term the oracle should watch.
#[rustfmt::skip]
const TERMS: &[Term] = &[
    // Document
    term("BT-1 invoice number", Text, true, ubl!("ID"), cii_doc!("ID"), fpa_doc!("Numero")),
    term("BT-2 issue date", Date, true, ubl!("IssueDate"), cii_doc!("IssueDateTime/DateTimeString"), fpa_doc!("Data")),
    term("BT-3 type code", Code, true, ubl!("InvoiceTypeCode"), cii_doc!("TypeCode"), ""),
    term("BT-5 currency", Code, true, ubl!("DocumentCurrencyCode"), cii_set!("InvoiceCurrencyCode"), fpa_doc!("Divisa")),
    term("BT-10 buyer reference", Text, true, ubl!("BuyerReference"), cii_agr!("BuyerReference"), ""),
    term("BT-13 purchase order", Text, true, ubl!("OrderReference/ID"), cii_agr!("BuyerOrderReferencedDocument/IssuerAssignedID"), ""),
    term("BT-25 preceding invoice", Text, false, ubl!("BillingReference/InvoiceDocumentReference/ID"), cii_set!("InvoiceReferencedDocument/IssuerAssignedID"), ""),
    // Parties
    term("BT-27 seller name", Text, true, ubl!("AccountingSupplierParty/Party/PartyLegalEntity/RegistrationName"), cii_agr!("SellerTradeParty/Name"), fpa_hdr!("CedentePrestatore/DatiAnagrafici/Anagrafica/Denominazione")),
    // One path holds both; the TaxScheme/scheme tells them apart.
    term("BT-31/32 seller VAT/tax id", Code, false, ubl!("AccountingSupplierParty/Party/PartyTaxScheme/CompanyID"), cii_agr!("SellerTradeParty/SpecifiedTaxRegistration/ID"), ""),
    term("BT-40 seller country", Code, true, ubl!("AccountingSupplierParty/Party/PostalAddress/Country/IdentificationCode"), cii_agr!("SellerTradeParty/PostalTradeAddress/CountryID"), fpa_hdr!("CedentePrestatore/Sede/Nazione")),
    term("BT-44 buyer name", Text, true, ubl!("AccountingCustomerParty/Party/PartyLegalEntity/RegistrationName"), cii_agr!("BuyerTradeParty/Name"), fpa_hdr!("CessionarioCommittente/DatiAnagrafici/Anagrafica/Denominazione")),
    term("BT-55 buyer country", Code, true, ubl!("AccountingCustomerParty/Party/PostalAddress/Country/IdentificationCode"), cii_agr!("BuyerTradeParty/PostalTradeAddress/CountryID"), fpa_hdr!("CessionarioCommittente/Sede/Nazione")),
    // Payment
    term("BT-84 payment account", Code, false, ubl!("PaymentMeans/PayeeFinancialAccount/ID"), cii_set!("SpecifiedTradeSettlementPaymentMeans/PayeePartyCreditorFinancialAccount/IBANID"), ""),
    // Document allowances and charges (BT-92, BT-99) and their indicator
    term("BT-92/99 doc allowance/charge amount", Number, false, ubl!("AllowanceCharge/Amount"), cii_set!("SpecifiedTradeAllowanceCharge/ActualAmount"), ""),
    term("BG-20/21 doc allowance/charge indicator", Code, false, ubl!("AllowanceCharge/ChargeIndicator"), cii_set!("SpecifiedTradeAllowanceCharge/ChargeIndicator/Indicator"), ""),
    // Totals
    term("BT-106 sum of line net", Number, true, ubl!("LegalMonetaryTotal/LineExtensionAmount"), cii_sum!("LineTotalAmount"), ""),
    term("BT-107 sum of allowances", Number, true, ubl!("LegalMonetaryTotal/AllowanceTotalAmount"), cii_sum!("AllowanceTotalAmount"), ""),
    term("BT-108 sum of charges", Number, true, ubl!("LegalMonetaryTotal/ChargeTotalAmount"), cii_sum!("ChargeTotalAmount"), ""),
    term("BT-109 total without VAT", Number, true, ubl!("LegalMonetaryTotal/TaxExclusiveAmount"), cii_sum!("TaxBasisTotalAmount"), ""),
    term("BT-110/111 total VAT", Number, false, ubl!("TaxTotal/TaxAmount"), cii_sum!("TaxTotalAmount"), ""),
    term("BT-112 total with VAT", Number, true, ubl!("LegalMonetaryTotal/TaxInclusiveAmount"), cii_sum!("GrandTotalAmount"), fpa_doc!("ImportoTotaleDocumento")),
    term("BT-113 paid amount", Number, true, ubl!("LegalMonetaryTotal/PrepaidAmount"), cii_sum!("TotalPrepaidAmount"), ""),
    term("BT-114 rounding amount", Number, true, ubl!("LegalMonetaryTotal/PayableRoundingAmount"), cii_sum!("RoundingAmount"), ""),
    term("BT-115 amount due", Number, true, ubl!("LegalMonetaryTotal/PayableAmount"), cii_sum!("DuePayableAmount"), ""),
    // VAT breakdown (BG-23)
    term("BT-116 VAT category taxable", Number, false, ubl!("TaxTotal/TaxSubtotal/TaxableAmount"), cii_set!("ApplicableTradeTax/BasisAmount"), fpa_goods!("DatiRiepilogo/ImponibileImporto")),
    term("BT-117 VAT category amount", Number, false, ubl!("TaxTotal/TaxSubtotal/TaxAmount"), cii_set!("ApplicableTradeTax/CalculatedAmount"), fpa_goods!("DatiRiepilogo/Imposta")),
    term("BT-118 VAT category code", Code, false, ubl!("TaxTotal/TaxSubtotal/TaxCategory/ID"), cii_set!("ApplicableTradeTax/CategoryCode"), ""),
    term("BT-119 VAT category rate", Number, false, ubl!("TaxTotal/TaxSubtotal/TaxCategory/Percent"), cii_set!("ApplicableTradeTax/RateApplicablePercent"), fpa_goods!("DatiRiepilogo/AliquotaIVA")),
    // Lines (BG-25), in document order
    term("BT-126 line id", Text, true, ubl!("InvoiceLine/ID"), cii_line!("AssociatedDocumentLineDocument/LineID"), fpa_goods!("DettaglioLinee/NumeroLinea")),
    term("BT-129 quantity", Number, true, ubl!("InvoiceLine/InvoicedQuantity"), cii_line!("SpecifiedLineTradeDelivery/BilledQuantity"), fpa_goods!("DettaglioLinee/Quantita")),
    term("BT-130 unit", Code, true, ubl!("InvoiceLine/InvoicedQuantity/@unitCode"), cii_line!("SpecifiedLineTradeDelivery/BilledQuantity/@unitCode"), ""),
    term("BT-131 line net amount", Number, true, ubl!("InvoiceLine/LineExtensionAmount"), cii_line!("SpecifiedLineTradeSettlement/SpecifiedTradeSettlementLineMonetarySummation/LineTotalAmount"), fpa_goods!("DettaglioLinee/PrezzoTotale")),
    term("BT-146 net price", Number, true, ubl!("InvoiceLine/Price/PriceAmount"), cii_line!("SpecifiedLineTradeAgreement/NetPriceProductTradePrice/ChargeAmount"), ""),
    term("BT-151 line VAT category", Code, true, ubl!("InvoiceLine/Item/ClassifiedTaxCategory/ID"), cii_line!("SpecifiedLineTradeSettlement/ApplicableTradeTax/CategoryCode"), ""),
    term("BT-152 line VAT rate", Number, true, ubl!("InvoiceLine/Item/ClassifiedTaxCategory/Percent"), cii_line!("SpecifiedLineTradeSettlement/ApplicableTradeTax/RateApplicablePercent"), fpa_goods!("DettaglioLinee/AliquotaIVA")),
    term("BT-153 item name", Text, true, ubl!("InvoiceLine/Item/Name"), cii_line!("SpecifiedTradeProduct/Name"), fpa_goods!("DettaglioLinee/Descrizione")),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Syntax {
    Ubl,
    Cii,
    FatturaPa,
}

impl Term {
    fn path(&self, syntax: Syntax) -> &'static str {
        match syntax {
            Syntax::Ubl => self.ubl,
            Syntax::Cii => self.cii,
            Syntax::FatturaPa => self.fpa,
        }
    }

    fn normalize(&self, raw: &str) -> String {
        let raw = raw.trim();
        match self.kind {
            Text => raw.split_whitespace().collect::<Vec<_>>().join(" "),
            Code => raw.to_uppercase(),
            Date => raw.replace('-', ""),
            Number => Decimal::from_str(raw)
                .map(|d| d.normalize().to_string())
                .unwrap_or_else(|_| format!("<not a number: {raw}>")),
        }
    }
}

/// A document as the oracle sees it: its syntax and every element's text and
/// attribute value by path, in document order.
struct Doc {
    syntax: Syntax,
    values: BTreeMap<String, Vec<String>>,
}

impl Doc {
    fn parse(xml: &str) -> Result<Doc, String> {
        let mut reader = Reader::from_str(xml);
        let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut stack: Vec<String> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut root = None;
        let path = |stack: &[String]| format!("/{}", stack.join("/"));
        loop {
            match reader.read_event().map_err(|e| e.to_string())? {
                ev @ (Event::Start(_) | Event::Empty(_)) => {
                    let (Event::Start(e) | Event::Empty(e)) = &ev else {
                        unreachable!()
                    };
                    let name = e.local_name().as_ref().to_string();
                    root.get_or_insert_with(|| name.clone());
                    stack.push(name);
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        if attr.key.as_ref().starts_with("xmlns") {
                            continue;
                        }
                        let key = attr.key.local_name().as_ref().to_string();
                        let value = attr
                            .normalized_value(XmlVersion::Implicit1_0)
                            .map_err(|e| e.to_string())?;
                        values
                            .entry(format!("{}/@{key}", path(&stack)))
                            .or_default()
                            .push(value.into_owned());
                    }
                    if matches!(ev, Event::Empty(_)) {
                        stack.pop();
                    } else {
                        texts.push(String::new());
                    }
                }
                // Text outside the root is the whitespace around it: ignored.
                Event::Text(t) => {
                    if let Some(text) = texts.last_mut() {
                        text.push_str(&t.xml10_content());
                    }
                }
                Event::CData(t) => {
                    if let Some(text) = texts.last_mut() {
                        text.push_str(&t.xml10_content());
                    }
                }
                Event::GeneralRef(r) => {
                    let text = texts.last_mut().ok_or("reference outside the root")?;
                    match r.resolve_char_ref().map_err(|e| e.to_string())? {
                        Some(c) => text.push(c),
                        None => {
                            let name = r.xml10_content();
                            text.push_str(
                                resolve_predefined_entity(&name)
                                    .ok_or(format!("unknown entity &{name};"))?,
                            );
                        }
                    }
                }
                Event::End(_) => {
                    let text = texts.pop().unwrap_or_default();
                    if !text.trim().is_empty() {
                        values.entry(path(&stack)).or_default().push(text);
                    }
                    stack.pop();
                }
                Event::Eof => break,
                _ => {}
            }
        }
        let syntax = match root.as_deref() {
            Some("Invoice") => Syntax::Ubl,
            Some("CrossIndustryInvoice") => Syntax::Cii,
            Some("FatturaElettronica") => Syntax::FatturaPa,
            other => return Err(format!("no oracle for root element {other:?}")),
        };
        Ok(Doc { syntax, values })
    }

    /// The term's normalized values; `None` when this syntax cannot carry it.
    /// Unordered terms come back sorted, so they compare as multisets.
    fn get(&self, term: &Term) -> Option<Vec<String>> {
        let path = term.path(self.syntax);
        if path.is_empty() {
            return None;
        }
        let mut values: Vec<String> = self
            .values
            .get(path)
            .into_iter()
            .flatten()
            .map(|v| term.normalize(v))
            .collect();
        if !term.ordered {
            values.sort();
        }
        Some(values)
    }

    fn by_bt(&self, bt_prefix: &str) -> Option<Vec<String>> {
        let term = TERMS
            .iter()
            .find(|t| t.bt.starts_with(bt_prefix))
            .expect("term in TERMS");
        self.get(term)
    }
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// Whether `got` carries `expected`. FatturaPA allows only Latin-1, so text
/// that passed through it may have its other characters transliterated
/// (`—` → `-`): with `latin1`, those match any run of characters.
fn agrees(term: &Term, expected: &[String], got: &[String], latin1: bool) -> bool {
    if expected == got {
        return true;
    }
    if !latin1 || !matches!(term.kind, Text) || expected.len() != got.len() {
        return false;
    }
    expected.iter().zip(got).all(|(e, g)| {
        let mut rest = g.as_str();
        let mut segments = e.split(|c: char| u32::from(c) > 0xFF).peekable();
        let first = segments.next().unwrap_or("");
        let Some(after) = rest.strip_prefix(first) else {
            return false;
        };
        rest = after;
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                return rest.ends_with(segment);
            }
            match rest.find(segment) {
                Some(i) => rest = &rest[i + segment.len()..],
                None => return false,
            }
        }
        rest.is_empty()
    })
}

/// Check 1: every term the source carries arrives unchanged.
fn fidelity(source: &Doc, output: &Doc) -> Vec<String> {
    let mut failures = Vec::new();
    for term in TERMS {
        let (Some(src), Some(out)) = (source.get(term), output.get(term)) else {
            continue; // one of the syntaxes cannot carry the term
        };
        if src.is_empty() || agrees(term, &src, &out, output.syntax == Syntax::FatturaPa) {
            continue; // nothing to carry (a derived value is check 2's job), or carried
        }
        if out.is_empty() {
            failures.push(format!("{}: dropped (source {src:?})", term.bt));
        } else {
            failures.push(format!(
                "{}: changed, source {src:?} -> output {out:?}",
                term.bt
            ));
        }
    }
    failures
}

fn decimals(values: Option<Vec<String>>) -> Option<Vec<Decimal>> {
    values?.iter().map(|v| Decimal::from_str(v).ok()).collect()
}

fn one(doc: &Doc, bt: &str) -> Option<Decimal> {
    match decimals(doc.by_bt(bt))?.as_slice() {
        [v] => Some(*v),
        _ => None,
    }
}

fn sum(doc: &Doc, bt: &str) -> Option<Decimal> {
    decimals(doc.by_bt(bt)).map(|v| v.iter().sum())
}

/// Check 2: the EN 16931 calculation rules the oracle can see, as
/// `(rule, holds)`; a rule whose operands are absent is not listed.
fn totals(doc: &Doc) -> Vec<(String, bool)> {
    let mut rules = Vec::new();
    let zero = Decimal::ZERO;
    if doc.syntax == Syntax::FatturaPa {
        return rules; // FatturaPA has no BR-CO totals to hold it to
    }
    let mut rule = |name: &str, carried: Option<Decimal>, computed: Option<Decimal>| {
        if let (Some(c), Some(k)) = (carried, computed) {
            rules.push((format!("{name}: carried {c}, computed {k}"), c == k));
        }
    };
    let (b106, b109, b112) = (one(doc, "BT-106"), one(doc, "BT-109"), one(doc, "BT-112"));
    rule("BR-CO-10 BT-106 = Σ BT-131", b106, sum(doc, "BT-131"));
    rule(
        "BR-CO-13 BT-109 = BT-106 − BT-107 + BT-108",
        b109,
        b106.map(|b| b - one(doc, "BT-107").unwrap_or(zero) + one(doc, "BT-108").unwrap_or(zero)),
    );
    // BT-110 only when the document states one VAT total (no BT-111).
    let b110 = one(doc, "BT-110");
    rule("BR-CO-14 BT-110 = Σ BT-117", b110, sum(doc, "BT-117"));
    rule(
        "BR-CO-15 BT-112 = BT-109 + BT-110",
        b112,
        b109.zip(b110).map(|(a, b)| a + b),
    );
    rule(
        "BR-CO-16 BT-115 = BT-112 − BT-113 + BT-114",
        one(doc, "BT-115"),
        b112.map(|b| b - one(doc, "BT-113").unwrap_or(zero) + one(doc, "BT-114").unwrap_or(zero)),
    );
    // Per VAT category: BT-117 = round(BT-116 × BT-119 / 100), ±0.01. The
    // three lists are sorted independently, so pair them only when unambiguous.
    if let (Some(basis), Some(amount), Some(rate)) = (
        decimals(doc.by_bt("BT-116")),
        decimals(doc.by_bt("BT-117")),
        decimals(doc.by_bt("BT-119")),
    ) && basis.len() == 1
        && amount.len() == 1
        && rate.len() == 1
    {
        let computed = (basis[0] * rate[0] / Decimal::ONE_HUNDRED).round_dp(2);
        rules.push((
            format!(
                "BT-117 = BT-116 × BT-119 / 100: carried {}, computed {computed}",
                amount[0]
            ),
            (amount[0] - computed).abs() <= Decimal::new(1, 2),
        ));
    }
    rules
}

/// Check 5: source → A → B against source → B. A term both carry must agree;
/// a term only the chain carries was invented on the way. `latin1`: the
/// chain passed through FatturaPA (see [`agrees`]).
fn independence(direct: &Doc, chained: &Doc, latin1: bool) -> Vec<String> {
    let mut failures = Vec::new();
    for term in TERMS {
        let (Some(d), Some(c)) = (direct.get(term), chained.get(term)) else {
            continue;
        };
        if c.is_empty() || agrees(term, &d, &c, latin1) {
            continue; // dropped by a lossy hop, or agrees
        }
        // An unordered term may lose some of its values on a lossy hop.
        let mut left = d.clone();
        if !term.ordered
            && c.iter().all(|v| {
                left.iter()
                    .position(|x| x == v)
                    .map(|i| left.remove(i))
                    .is_some()
            })
        {
            continue;
        }
        if d.is_empty() {
            failures.push(format!("{}: only the chain carries it: {c:?}", term.bt));
        } else {
            failures.push(format!("{}: direct {d:?}, chained {c:?}", term.bt));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every source: the declared samples, then `KRAB_CORPUS_DIR`'s documents.
fn sources() -> Vec<(String, Spoke, Vec<u8>)> {
    let mut sources = Vec::new();
    for &spoke in Spoke::ALL {
        for &file in spoke.samples() {
            let bytes =
                std::fs::read(workspace().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            sources.push((file.to_string(), spoke, bytes));
        }
    }
    if let Some(dir) = std::env::var_os("KRAB_CORPUS_DIR") {
        // Relative to the workspace root, like the sample paths.
        let mut files: Vec<PathBuf> = walk(&workspace().join(dir));
        files.sort();
        for file in files {
            let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            match detect_source(&bytes) {
                Ok(spoke) => sources.push((file.display().to_string(), spoke, bytes)),
                Err(e) => eprintln!("skipped {}: {e}", file.display()),
            }
        }
    }
    sources
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("KRAB_CORPUS_DIR {}: {e}", dir.display()));
    entries
        .flatten()
        .flat_map(|e| {
            let p = e.path();
            if p.is_dir() {
                walk(&p)
            } else if p.extension().is_some_and(|x| x == "xml") {
                vec![p]
            } else {
                vec![]
            }
        })
        .collect()
}

/// The transform's output, or `None` when the target refuses it.
fn transform(from: Spoke, to: Spoke, bytes: &[u8]) -> Option<String> {
    let result = Engine::new().transform(from, to, bytes).ok()?;
    if result.has_errors() {
        return None;
    }
    result.value
}

#[test]
fn test_transformations_carry_their_source_correctly() {
    let mut failures: Vec<String> = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let mut checked = 0;
    let sources = sources();
    assert!(
        !sources.is_empty(),
        "no mapping declares a [[meta.samples]] document"
    );

    for (name, reader, bytes) in &sources {
        let source = match std::str::from_utf8(bytes)
            .map_err(|e| e.to_string())
            .and_then(Doc::parse)
        {
            Ok(doc) => doc,
            Err(e) => {
                // Never skip silently: an unreadable source would pass vacuously.
                failures.push(format!("{name}: the oracle cannot read the source: {e}"));
                continue;
            }
        };
        let mut fail = |what: String| failures.push(format!("{name}: {what}"));
        for (rule, holds) in totals(&source) {
            if !holds {
                report.push(format!("{name}: source itself violates {rule}"));
            }
        }
        let source_broken: Vec<String> = totals(&source)
            .into_iter()
            .filter(|(_, h)| !h)
            .map(|(r, _)| rule_id(&r))
            .collect();

        // Direct transforms, kept for the chains.
        let mut outputs: Vec<(Spoke, String, Doc)> = Vec::new();
        for &target in Spoke::ALL {
            let pair = format!("{} -> {}", reader.name(), target.name());
            let Some(out) = transform(*reader, target, bytes) else {
                report.push(format!("{name}: {pair}: refused, skipped"));
                continue;
            };
            // 3. Determinism.
            if transform(*reader, target, bytes).as_deref() != Some(out.as_str()) {
                fail(format!("{pair}: not deterministic"));
            }
            // 4. Idempotence.
            match transform(target, target, out.as_bytes()) {
                Some(again) if again == out => {}
                Some(_) => fail(format!(
                    "{pair}: re-transforming the output into {} changes it",
                    target.name()
                )),
                None => fail(format!("{pair}: {} refuses its own output", target.name())),
            }
            let doc = match Doc::parse(&out) {
                Ok(doc) => doc,
                Err(e) => {
                    fail(format!("{pair}: output unreadable by the oracle: {e}"));
                    continue;
                }
            };
            // 1. Fidelity.
            for f in fidelity(&source, &doc) {
                fail(format!("{pair}: {f}"));
            }
            // 2. Totals, where the source holds them.
            let rules = totals(&doc);
            for (rule, holds) in &rules {
                if !holds && !source_broken.contains(&rule_id(rule)) {
                    fail(format!("{pair}: output violates {rule}"));
                }
            }
            report.push(format!(
                "{name}: {pair}: checked, {} total rule(s)",
                rules.len()
            ));
            checked += 1;
            outputs.push((target, out, doc));
        }

        // 5. Path independence over every intermediate format.
        for (via, via_out, via_doc) in &outputs {
            for (target, _, direct) in &outputs {
                let Some(chained) = transform(*via, *target, via_out.as_bytes()) else {
                    continue;
                };
                let chained = match Doc::parse(&chained) {
                    Ok(doc) => doc,
                    Err(e) => {
                        fail(format!(
                            "{} -> {} -> {}: unreadable: {e}",
                            reader.name(),
                            via.name(),
                            target.name()
                        ));
                        continue;
                    }
                };
                let latin1 =
                    via_doc.syntax == Syntax::FatturaPa || direct.syntax == Syntax::FatturaPa;
                for f in independence(direct, &chained, latin1) {
                    fail(format!(
                        "{} -> {} -> {}: {f}",
                        reader.name(),
                        via.name(),
                        target.name()
                    ));
                }
            }
        }
    }

    eprintln!("{}", report.join("\n"));
    // Not vacuous: every source reaches at least its own format.
    assert!(
        checked >= sources.len(),
        "only {checked} transform(s) checked for {} source(s)",
        sources.len()
    );
    assert!(
        failures.is_empty(),
        "\n{} failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A rule's id, its text up to the first `:` (`"BR-CO-10 BT-106 = Σ BT-131"`).
fn rule_id(rule: &str) -> String {
    rule.split(':').next().unwrap_or(rule).to_string()
}

// ---------------------------------------------------------------------------
// The oracle's own checks: a broken oracle would pass everything.
// ---------------------------------------------------------------------------

const UBL_DOC: &str = r#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2" xmlns:cbc="urn:x"><cbc:ID>A&amp;B</cbc:ID><IssueDate>2026-01-31</IssueDate>
  <LegalMonetaryTotal><LineExtensionAmount>100.00</LineExtensionAmount><TaxExclusiveAmount>100</TaxExclusiveAmount><TaxInclusiveAmount>119</TaxInclusiveAmount><PayableAmount>119.00</PayableAmount></LegalMonetaryTotal>
  <TaxTotal><TaxAmount>19</TaxAmount><TaxSubtotal><TaxableAmount>100</TaxableAmount><TaxAmount>19</TaxAmount><TaxCategory><Percent>19</Percent></TaxCategory></TaxSubtotal></TaxTotal>
  <InvoiceLine><ID>1</ID><InvoicedQuantity unitCode="c62">2</InvoicedQuantity><LineExtensionAmount>60</LineExtensionAmount></InvoiceLine>
  <InvoiceLine><ID>2</ID><InvoicedQuantity unitCode="C62">1</InvoicedQuantity><LineExtensionAmount>40</LineExtensionAmount></InvoiceLine></Invoice>"#;

const CII_DOC: &str = r#"<rsm:CrossIndustryInvoice xmlns:rsm="urn:r" xmlns:ram="urn:a"><rsm:ExchangedDocument><ram:ID>A&amp;B</ram:ID><ram:IssueDateTime><udt:DateTimeString xmlns:udt="urn:u" format="102">20260131</udt:DateTimeString></ram:IssueDateTime></rsm:ExchangedDocument>
  <rsm:SupplyChainTradeTransaction>
    <ram:IncludedSupplyChainTradeLineItem><ram:AssociatedDocumentLineDocument><ram:LineID>1</ram:LineID></ram:AssociatedDocumentLineDocument><ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="C62">2.000</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery><ram:SpecifiedLineTradeSettlement><ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>60</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation></ram:SpecifiedLineTradeSettlement></ram:IncludedSupplyChainTradeLineItem>
    <ram:IncludedSupplyChainTradeLineItem><ram:AssociatedDocumentLineDocument><ram:LineID>2</ram:LineID></ram:AssociatedDocumentLineDocument><ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="C62">1</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery><ram:SpecifiedLineTradeSettlement><ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>40.0</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation></ram:SpecifiedLineTradeSettlement></ram:IncludedSupplyChainTradeLineItem>
    <ram:ApplicableHeaderTradeSettlement><ram:ApplicableTradeTax><ram:CalculatedAmount>19</ram:CalculatedAmount><ram:BasisAmount>100</ram:BasisAmount><ram:RateApplicablePercent>19</ram:RateApplicablePercent></ram:ApplicableTradeTax>
      <ram:SpecifiedTradeSettlementHeaderMonetarySummation><ram:LineTotalAmount>100</ram:LineTotalAmount><ram:TaxBasisTotalAmount>100</ram:TaxBasisTotalAmount><ram:TaxTotalAmount>19</ram:TaxTotalAmount><ram:GrandTotalAmount>119</ram:GrandTotalAmount><ram:DuePayableAmount>119</ram:DuePayableAmount></ram:SpecifiedTradeSettlementHeaderMonetarySummation></ram:ApplicableHeaderTradeSettlement>
  </rsm:SupplyChainTradeTransaction></rsm:CrossIndustryInvoice>"#;

#[test]
fn test_oracle_sees_equivalent_ubl_and_cii_as_equal() {
    let (ubl, cii) = (Doc::parse(UBL_DOC).unwrap(), Doc::parse(CII_DOC).unwrap());
    assert_eq!((ubl.syntax, cii.syntax), (Syntax::Ubl, Syntax::Cii));
    assert_eq!(
        ubl.by_bt("BT-1"),
        Some(vec!["A&B".to_string()]),
        "entities resolve"
    );
    assert_eq!(fidelity(&ubl, &cii), Vec::<String>::new());
    assert_eq!(fidelity(&cii, &ubl), Vec::<String>::new());
    assert!(
        totals(&ubl).iter().all(|(_, holds)| *holds),
        "{:?}",
        totals(&ubl)
    );
    assert_eq!(
        totals(&cii).len(),
        6,
        "every rule with operands is checked: {:?}",
        totals(&cii)
    );
}

#[test]
fn test_oracle_catches_changed_dropped_reordered_and_inconsistent() {
    let ubl = Doc::parse(UBL_DOC).unwrap();
    let changed =
        Doc::parse(&UBL_DOC.replace("<PayableAmount>119.00", "<PayableAmount>119.01")).unwrap();
    let dropped = Doc::parse(&UBL_DOC.replace("<IssueDate>2026-01-31</IssueDate>", "")).unwrap();
    let swapped = Doc::parse(
        &UBL_DOC
            .replace("<ID>1</ID>", "<ID>X</ID>")
            .replace("<ID>2</ID>", "<ID>1</ID>")
            .replace("<ID>X</ID>", "<ID>2</ID>"),
    )
    .unwrap();

    assert_eq!(
        fidelity(&ubl, &changed),
        vec!["BT-115 amount due: changed, source [\"119\"] -> output [\"119.01\"]"]
    );
    assert_eq!(
        fidelity(&ubl, &dropped),
        vec!["BT-2 issue date: dropped (source [\"20260131\"])"]
    );
    assert_eq!(
        fidelity(&ubl, &swapped).len(),
        1,
        "line order is significant"
    );
    let broken: Vec<_> = totals(&changed).into_iter().filter(|(_, h)| !h).collect();
    assert_eq!(broken.len(), 1, "{broken:?}");
    assert!(broken[0].0.starts_with("BR-CO-16"), "{broken:?}");
    assert_eq!(independence(&ubl, &changed, false).len(), 1);
    assert_eq!(
        independence(&ubl, &dropped, false),
        Vec::<String>::new(),
        "a lossy hop may drop"
    );
}

#[test]
fn test_latin1_transliteration_matches_only_where_characters_left_latin1() {
    let name = &TERMS.iter().find(|t| t.bt.starts_with("BT-153")).unwrap();
    let src = ["Beratung — Senior".to_string()];
    for ok in [
        "Beratung - Senior",
        "Beratung -- Senior",
        "Beratung  Senior",
    ] {
        assert!(agrees(name, &src, &[ok.to_string()], true), "{ok}");
    }
    for bad in [
        "Beratung - Junior",
        "Beratung - Senior!",
        "Xeratung - Senior",
    ] {
        assert!(!agrees(name, &src, &[bad.to_string()], true), "{bad}");
    }
    assert!(
        !agrees(name, &src, &["Beratung - Senior".to_string()], false),
        "only through FatturaPA"
    );
    let umlaut = ["Müller".to_string()];
    assert!(
        !agrees(name, &umlaut, &["Muller".to_string()], true),
        "Latin-1 must survive"
    );
}
