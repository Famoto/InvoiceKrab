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
use einvoice_transformator::result::Severity;
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

/// The raw decimals at a path, in document order (unparsable values skipped).
fn raw_decimals(doc: &Doc, path: &str) -> Vec<Decimal> {
    let values = doc.values.get(path).into_iter().flatten();
    values
        .filter_map(|v| Decimal::from_str(v.trim()).ok())
        .collect()
}

/// FatturaPA's own consistency rule (SdI check 00422): per VAT rate, the
/// summary's `ImponibileImporto` is the sum of the lines' `PrezzoTotale` at
/// that rate plus the summary's `Arrotondamento`. Rates are summed rather
/// than matched one to one, since one rate may have several summaries (one
/// per `Natura`). Line and summary fields are mandatory per element, so the
/// lists align by position; when they do not, the rule is not listed.
fn fatturapa_totals(doc: &Doc) -> Vec<(String, bool)> {
    let goods = "/FatturaElettronica/FatturaElettronicaBody/DatiBeniServizi";
    let line_rate = raw_decimals(doc, &format!("{goods}/DettaglioLinee/AliquotaIVA"));
    let line_total = raw_decimals(doc, &format!("{goods}/DettaglioLinee/PrezzoTotale"));
    let sum_rate = raw_decimals(doc, &format!("{goods}/DatiRiepilogo/AliquotaIVA"));
    let sum_base = raw_decimals(doc, &format!("{goods}/DatiRiepilogo/ImponibileImporto"));
    if line_rate.len() != line_total.len()
        || sum_rate.len() != sum_base.len()
        || sum_rate.is_empty()
    {
        return Vec::new();
    }
    // Arrotondamento is optional, so it can only be attributed when every
    // summary carries one; otherwise it is left out (and may show as a gap).
    let rounding = raw_decimals(doc, &format!("{goods}/DatiRiepilogo/Arrotondamento"));
    let rounding = if rounding.len() == sum_rate.len() {
        rounding
    } else {
        vec![Decimal::ZERO; sum_rate.len()]
    };
    let mut by_rate: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for (rate, total) in line_rate.iter().zip(&line_total) {
        by_rate.entry(rate.normalize().to_string()).or_default().1 += total;
    }
    for ((rate, base), round) in sum_rate.iter().zip(&sum_base).zip(&rounding) {
        let entry = by_rate.entry(rate.normalize().to_string()).or_default();
        entry.0 += base;
        entry.1 += round;
    }
    by_rate
        .into_iter()
        .map(|(rate, (base, lines))| {
            (
                format!("FatturaPA 00422 ImponibileImporto at {rate}% = Σ PrezzoTotale + Arrotondamento: carried {base}, computed {lines}"),
                base == lines,
            )
        })
        .collect()
}

/// Check 2: the EN 16931 calculation rules the oracle can see (FatturaPA's
/// own rule for FatturaPA), as `(rule, holds)`; a rule whose operands are
/// absent is not listed.
fn totals(doc: &Doc) -> Vec<(String, bool)> {
    if doc.syntax == Syntax::FatturaPa {
        return fatturapa_totals(doc);
    }
    let mut rules = Vec::new();
    let zero = Decimal::ZERO;
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

/// Every source: the declared samples, then `KRAB_CORPUS_DIR`'s documents
/// (flagged `true`).
fn sources() -> Vec<(String, Spoke, Vec<u8>, bool)> {
    let mut sources = Vec::new();
    for &spoke in Spoke::ALL {
        for &file in spoke.samples() {
            let bytes =
                std::fs::read(workspace().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            sources.push((file.to_string(), spoke, bytes, false));
        }
    }
    if let Some(dir) = std::env::var_os("KRAB_CORPUS_DIR") {
        // Relative to the workspace root, like the sample paths.
        let mut files: Vec<PathBuf> = walk(&workspace().join(dir));
        files.sort();
        for file in files {
            let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            match detect_source(&bytes) {
                Ok(spoke) => sources.push((file.display().to_string(), spoke, bytes, true)),
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
        .flat_map(|e| {
            let p = e
                .unwrap_or_else(|e| panic!("KRAB_CORPUS_DIR {}: {e}", dir.display()))
                .path();
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
    refusal(from, to, bytes).ok()
}

/// The transform's output, or why it was refused.
fn refusal(from: Spoke, to: Spoke, bytes: &[u8]) -> Result<String, String> {
    let result = Engine::new()
        .transform(from, to, bytes)
        .map_err(|e| e.to_string())?;
    if let Some(d) = result
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
    {
        return Err(format!("[{}] {}", d.code, d.message));
    }
    result.value.ok_or_else(|| "no output".to_string())
}

#[test]
fn test_transformations_carry_their_source_correctly() {
    let mut failures: Vec<Failure> = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let sources = sources();
    assert!(
        !sources.is_empty(),
        "no mapping declares a [[meta.samples]] document"
    );

    for (name, reader, bytes, corpus) in &sources {
        let source = match std::str::from_utf8(bytes)
            .map_err(|e| e.to_string())
            .and_then(Doc::parse)
        {
            Ok(doc) => doc,
            Err(e) => {
                // Never skip silently: an unreadable source would pass vacuously.
                failures.push(Failure {
                    text: format!("{name}: the oracle cannot read the source: {e}"),
                    corpus: *corpus,
                    doc_allowances: false,
                });
                continue;
            }
        };
        let doc_allowances = source
            .by_bt("BT-92/99")
            .is_some_and(|amounts| !amounts.is_empty());
        let mut fail = |what: String| {
            failures.push(Failure {
                text: format!("{name}: {what}"),
                corpus: *corpus,
                doc_allowances,
            })
        };
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
        let mut refusals: Vec<String> = Vec::new();
        for &target in Spoke::ALL {
            let pair = format!("{} -> {}", reader.name(), target.name());
            let out = match refusal(*reader, target, bytes) {
                Ok(out) => out,
                Err(why) => {
                    report.push(format!("{name}: {pair}: refused, skipped: {why}"));
                    refusals.push(why);
                    continue;
                }
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
            outputs.push((target, out, doc));
        }
        // Not vacuous: every source must reach at least one format.
        if outputs.is_empty() {
            refusals.dedup();
            fail(format!(
                "no transform checked, every target refuses it: {}",
                refusals.join("; ")
            ));
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

    let corpus = std::env::var_os("KRAB_CORPUS_DIR").is_some();
    let (known, failures, stale) = split_known(failures, corpus);
    report.extend(known.iter().map(|k| format!("known issue: {k}")));
    eprintln!("{}", report.join("\n"));
    assert!(
        stale.is_empty(),
        "\nknown issue(s) no failure shows any more (fixed? remove them from KNOWN_ISSUES):\n{}",
        stale.join("\n")
    );
    assert!(
        failures.is_empty(),
        "\n{} failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// One failed check, with the source context the known issues match on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    text: String,
    /// The source is a `KRAB_CORPUS_DIR` document, not a declared sample.
    corpus: bool,
    /// The source carries document-level allowances or charges (BG-20/21).
    doc_allowances: bool,
}

/// An engine issue this harness found that is not fixed yet: a failure line
/// containing every one of `matches` is reported as known instead of failing.
/// An issue no failure matches any more fails the test, so its fix is noticed
/// and the entry removed. A `corpus_only` issue matches only corpus failures
/// (it shows only with `KRAB_CORPUS_DIR`, and is only held to that then); a
/// `doc_allowances` one only failures of sources with BG-20/21.
struct KnownIssue {
    matches: &'static [&'static str],
    why: &'static str,
    corpus_only: bool,
    doc_allowances: bool,
}

impl KnownIssue {
    fn absorbs(&self, f: &Failure) -> bool {
        (f.corpus || !self.corpus_only)
            && (f.doc_allowances || !self.doc_allowances)
            && self.matches.iter().all(|m| f.text.contains(m))
    }
}

const KNOWN_ISSUES: &[KnownIssue] = &[
    KnownIssue {
        matches: &["-> fatturapa:1.2.2: output violates FatturaPA 00422"],
        why: "the FatturaPA writer drops document-level allowances and charges (BG-20, BG-21) \
              but counts them in DatiRiepilogo, so the lines no longer add up to \
              ImponibileImporto (SdI rejects that with 00422)",
        corpus_only: false,
        doc_allowances: true,
    },
    KnownIssue {
        matches: &["-> fatturapa:1.2.2 -> ", "BT-109 total without VAT"],
        why: "the same dropped document-level allowances and charges: read back from \
              FatturaPA, BT-109 is derived from the lines alone",
        corpus_only: false,
        doc_allowances: true,
    },
    KnownIssue {
        matches: &["-> fatturapa:1.2.2 -> ", "BT-113 paid amount"],
        why: "FatturaPA carries neither BT-113 nor BT-114: read back, BR-CO-16 derives the \
              paid amount as BT-112 - BT-115, so a rounding amount becomes a prepayment",
        corpus_only: false,
        doc_allowances: false,
    },
    KnownIssue {
        matches: &["BT-31/32 seller VAT/tax id"],
        why: "the UBL readers bind BT-32 only for PartyTaxScheme/TaxScheme/ID `FC` (`TAX` \
              for Peppol); EN 16931 allows any value but `VAT`, so others (KoSIT uses `???` \
              and `ID`) are dropped without a diagnostic",
        corpus_only: true,
        doc_allowances: false,
    },
    KnownIssue {
        matches: &[
            "no transform checked",
            "source deserialization failed: duplicate field",
        ],
        why: "the UBL readers bind TaxTotal, BillingReference and \
              CommodityClassification as single elements, so a document repeating one \
              (as UBL and EN 16931 allow) is not read at all",
        corpus_only: true,
        doc_allowances: false,
    },
    KnownIssue {
        matches: &[
            "GR-base-example-TaxRepresentative.xml: no transform checked",
            "[REQUIRED_MISSING]",
        ],
        why: "the Peppol reader reports a required value missing in the Greek tax \
              representative example, so no target accepts it (not yet diagnosed)",
        corpus_only: true,
        doc_allowances: false,
    },
];

/// Splits `failures` into known issues (one line per issue, with its count),
/// the remaining failures, and the issues no failure matched any more.
fn split_known(failures: Vec<Failure>, corpus: bool) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut hits = vec![0usize; KNOWN_ISSUES.len()];
    let unknown = failures
        .into_iter()
        .filter_map(|f| match KNOWN_ISSUES.iter().position(|k| k.absorbs(&f)) {
            Some(i) => {
                hits[i] += 1;
                None
            }
            None => Some(f.text),
        })
        .collect();
    let mut known = Vec::new();
    let mut stale = Vec::new();
    for (issue, &n) in KNOWN_ISSUES.iter().zip(&hits) {
        if n > 0 {
            known.push(format!(
                "{} ({n} failure(s)): {}",
                issue.matches.join(" … "),
                issue.why
            ));
        } else if corpus || !issue.corpus_only {
            stale.push(format!("{}: {}", issue.matches.join(" … "), issue.why));
        }
    }
    (known, unknown, stale)
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
fn test_fatturapa_summary_must_equal_its_lines_per_rate() {
    let doc = |charge: &str| {
        format!(
            "<p:FatturaElettronica xmlns:p=\"urn:p\"><FatturaElettronicaBody><DatiBeniServizi>\
             <DettaglioLinee><PrezzoTotale>100.00</PrezzoTotale><AliquotaIVA>22.00</AliquotaIVA></DettaglioLinee>\
             <DettaglioLinee><PrezzoTotale>50.00</PrezzoTotale><AliquotaIVA>0.00</AliquotaIVA></DettaglioLinee>{charge}\
             <DatiRiepilogo><AliquotaIVA>22.00</AliquotaIVA><ImponibileImporto>125.00</ImponibileImporto></DatiRiepilogo>\
             <DatiRiepilogo><AliquotaIVA>0.00</AliquotaIVA><ImponibileImporto>50</ImponibileImporto></DatiRiepilogo>\
             </DatiBeniServizi></FatturaElettronicaBody></p:FatturaElettronica>"
        )
    };
    let charge = "<DettaglioLinee><PrezzoTotale>25</PrezzoTotale><AliquotaIVA>22</AliquotaIVA></DettaglioLinee>";
    let with_charge = Doc::parse(&doc(charge)).unwrap();
    assert_eq!(with_charge.syntax, Syntax::FatturaPa);
    let rules = totals(&with_charge);
    assert_eq!(rules.len(), 2, "{rules:?}");
    assert!(rules.iter().all(|(_, holds)| *holds), "{rules:?}");
    // The charge dropped from the lines, still counted in the summary.
    let broken: Vec<_> = totals(&Doc::parse(&doc("")).unwrap())
        .into_iter()
        .filter(|(_, h)| !h)
        .collect();
    assert_eq!(broken.len(), 1, "{broken:?}");
    assert!(
        broken[0].0.contains("at 22%") && broken[0].0.contains("carried 125.00, computed 100.00"),
        "{broken:?}"
    );
}

#[test]
fn test_known_issues_absorb_their_failures_and_go_stale_once_fixed() {
    let fpa = failure(
        "x.xml: ubl-invoice:2.1 -> fatturapa:1.2.2: output violates FatturaPA 00422 at 19%",
        false,
        true,
    );
    let other = "x.xml: ubl-invoice:2.1 -> facturx-invoice:1.0: BT-1 invoice number: dropped";
    let (known, unknown, stale) =
        split_known(vec![fpa.clone(), fpa, failure(other, false, true)], false);
    assert_eq!(
        unknown,
        vec![other.to_string()],
        "an unlisted failure still fails"
    );
    assert_eq!(known.len(), 1);
    assert!(known[0].contains("(2 failure(s))"), "{known:?}");
    // The other non-corpus issues matched nothing: stale. The corpus-only one
    // is not held to it without a corpus.
    let expected_stale = KNOWN_ISSUES.iter().filter(|k| !k.corpus_only).count() - 1;
    assert_eq!(stale.len(), expected_stale, "{stale:?}");
    let (_, _, stale) = split_known(Vec::new(), true);
    assert_eq!(
        stale.len(),
        KNOWN_ISSUES.len(),
        "with a corpus, every issue must show"
    );
}

fn failure(text: &str, corpus: bool, doc_allowances: bool) -> Failure {
    Failure {
        text: text.into(),
        corpus,
        doc_allowances,
    }
}

#[test]
fn test_corpus_only_issue_does_not_absorb_a_sample_failure() {
    let seller = "x.xml: ubl-invoice:2.1 -> cii:2.3: BT-31/32 seller VAT/tax id: dropped";
    let (known, unknown, _) = split_known(vec![failure(seller, false, false)], true);
    assert_eq!(
        unknown,
        vec![seller.to_string()],
        "a sample failure still fails"
    );
    assert!(known.is_empty(), "{known:?}");
    let (known, unknown, _) = split_known(vec![failure(seller, true, false)], true);
    assert!(unknown.is_empty(), "{unknown:?}");
    assert_eq!(known.len(), 1);
}

#[test]
fn test_fatturapa_00422_issue_needs_document_level_allowances() {
    // An 00422 mismatch from a source without BG-20/21 is not the dropped
    // allowances: it fails.
    let fpa = "x.xml: ubl-invoice:2.1 -> fatturapa:1.2.2: output violates FatturaPA 00422 at 19%";
    let (_, unknown, _) = split_known(vec![failure(fpa, false, false)], false);
    assert_eq!(unknown, vec![fpa.to_string()]);
    let bt109 =
        "x.xml: ubl-invoice:2.1 -> fatturapa:1.2.2 -> cii:2.3: BT-109 total without VAT: changed";
    let (_, unknown, _) = split_known(vec![failure(bt109, true, false)], true);
    assert_eq!(unknown, vec![bt109.to_string()]);
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

#[test]
fn test_fatturapa_refuses_a_seller_name_over_its_length_limit() {
    // Denominazione is a String80LatinType: 80 characters write, 81 are
    // refused (VALUE_TOO_LONG), never cut.
    let sample =
        std::fs::read_to_string(workspace().join("testfiles/en16931-full-ubl.xml")).unwrap();
    let named = |n: usize| {
        sample.replace(
            "<cbc:RegistrationName>Hafenkran Service GmbH</cbc:RegistrationName>",
            &format!(
                "<cbc:RegistrationName>{}</cbc:RegistrationName>",
                "x".repeat(n)
            ),
        )
    };
    assert_ne!(named(80), sample, "the sample still names its seller");
    let fits = refusal(Spoke::UblInvoice, Spoke::Fatturapa, named(80).as_bytes());
    assert!(fits.is_ok_and(|xml| xml.contains(&"x".repeat(80))));
    let over = refusal(Spoke::UblInvoice, Spoke::Fatturapa, named(81).as_bytes());
    assert!(
        over.as_ref()
            .is_err_and(|e| e.starts_with("[VALUE_TOO_LONG]")),
        "{over:?}"
    );
}

#[test]
fn test_repeated_payment_means_lift_the_shared_code_and_keep_every_account() {
    // BG-17 repeats per account; BT-81 is the invoice's, so every PaymentMeans
    // states it (a lifted key): read once, written into each occurrence.
    let sample =
        std::fs::read_to_string(workspace().join("testfiles/en16931-full-ubl.xml")).unwrap();
    let first = sample
        .split_once("  <cac:PaymentMeans>")
        .and_then(|(_, rest)| rest.split_once("</cac:PaymentMeans>\n"))
        .map(|(block, _)| format!("  <cac:PaymentMeans>{block}</cac:PaymentMeans>\n"))
        .expect("the sample has a PaymentMeans");
    let second = first.replace("DE02120300000000202051", "DE89370400440532013000");
    let two = sample.replacen(&first, &format!("{first}{second}"), 1);
    for to in [Spoke::UblInvoice, Spoke::FacturxInvoice] {
        let out = refusal(Spoke::UblInvoice, to, two.as_bytes()).unwrap();
        for iban in ["DE02120300000000202051", "DE89370400440532013000"] {
            assert!(out.contains(iban), "{to:?} keeps {iban}");
        }
        let code = if to == Spoke::UblInvoice {
            ">58</cbc:PaymentMeansCode>"
        } else {
            "<ram:TypeCode>58<"
        };
        assert_eq!(
            out.matches(code).count(),
            2,
            "{to:?}: the code in both occurrences"
        );
    }
    // Occurrences disagreeing on the invoice's payment means are refused.
    let disagree = sample.replacen(
        &first,
        &format!("{first}{}", second.replace(">58<", ">30<")),
        1,
    );
    let refused = refusal(Spoke::UblInvoice, Spoke::UblInvoice, disagree.as_bytes());
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.starts_with("[SCOPE_DISAGREE]")),
        "{refused:?}"
    );
    // A payment means without any account still gets its one occurrence.
    let code_only = sample.replace(
        &first,
        "  <cac:PaymentMeans>\n    <cbc:PaymentMeansCode>10</cbc:PaymentMeansCode>\n  </cac:PaymentMeans>\n",
    );
    let out = refusal(
        Spoke::UblInvoice,
        Spoke::FacturxInvoice,
        code_only.as_bytes(),
    )
    .unwrap();
    assert_eq!(
        out.matches("<ram:SpecifiedTradeSettlementPaymentMeans>")
            .count(),
        1
    );
    let back = refusal(Spoke::FacturxInvoice, Spoke::UblInvoice, out.as_bytes()).unwrap();
    assert!(back.contains(">10</cbc:PaymentMeansCode>"), "{back}");
}
