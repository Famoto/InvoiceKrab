//! Criterion benchmarks for the engine's transformation paths.
//!
//! Covers the three [`Engine`] entry points over **every** generated spoke as
//! both source and target:
//!
//! - `to_hub`    — source XML bytes → typed canonical hub (the read half).
//! - `from_hub`  — typed hub → target XML bytes (the write half).
//! - `transform` — the full N–1–N path (read + write through the hub), across
//!   the Cartesian product of source x target spokes, so same-spoke
//!   round-trips, the reverse direction, and the cross-format routes are all
//!   measured.
//!
//! Each path is measured against a small invoice (2 lines) and a large one
//! (200 lines) so a regression in either the per-document or the per-line cost
//! is visible. Inputs are built once outside the timing loop; only the engine
//! call is measured. Run with `cargo bench -p einvoice-interfaces`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use einvoice_interfaces::{Engine, MainKey, Spoke};
use std::hint::black_box;

/// Builds an invoice document of `lines` lines under the shared UBL `Invoice`
/// syntax. `extra_header` injects spoke-specific leading elements (e.g. the
/// XRechnung `CustomizationID`) right after the root open tag.
fn invoice(extra_header: &str, lines: usize) -> Vec<u8> {
    let mut xml = format!(
        "<Invoice>{extra_header}\
         <ID>INV-42</ID>\
         <IssueDate>2026-06-27</IssueDate>\
         <DocumentCurrencyCode>eur</DocumentCurrencyCode>\
         <LegalMonetaryTotal>\
         <PayableAmount currencyID=\"EUR\">119.00</PayableAmount>\
         </LegalMonetaryTotal>"
    );
    for i in 1..=lines {
        xml.push_str(&format!(
            "<InvoiceLine><ID>{i}</ID><InvoicedQuantity>{i}</InvoicedQuantity>\
             <Item><Name>Item {i}</Name></Item></InvoiceLine>"
        ));
    }
    xml.push_str("</Invoice>");
    xml.into_bytes()
}

/// A plain UBL invoice fixture with `lines` invoice lines.
fn ubl_invoice(lines: usize) -> Vec<u8> {
    invoice("", lines)
}

/// An XRechnung invoice fixture: UBL syntax plus the required CIUS
/// `CustomizationID` (BT-24) so the XRechnung reader sees a complete document.
fn xrechnung_invoice(lines: usize) -> Vec<u8> {
    invoice(
        "<CustomizationID>urn:cen.eu:en16931:2017#compliant#\
         urn:xoev-de:kosit:standard:xrechnung_3.0</CustomizationID>",
        lines,
    )
}

/// One benchmarked spoke: its label, the [`Spoke`], and a builder for a
/// document it can read (given a line count).
type SpokeCase = (&'static str, Spoke, fn(usize) -> Vec<u8>);

/// Every spoke under benchmark, paired with a builder for a document it can
/// read. Adding a spoke here extends all three benchmark groups.
const SPOKES: [SpokeCase; 2] = [
    ("ubl", Spoke::UblInvoice, ubl_invoice),
    ("xrechnung", Spoke::XrechnungInvoice, xrechnung_invoice),
];

/// The two input sizes exercised by every benchmark group.
const SIZES: [usize; 2] = [2, 200];

/// Reads `bytes` into a hub via `spoke`, panicking if it does not parse
/// cleanly. Used to prepare a hub for the `from_hub` benchmark outside the
/// timing loop.
fn hub_of(engine: &Engine, spoke: Spoke, bytes: &[u8]) -> MainKey {
    engine
        .to_hub(spoke, bytes)
        .expect("benchmark fixture is well-formed")
        .value
        .expect("reader always yields a hub")
}

fn bench_to_hub(c: &mut Criterion) {
    let engine = Engine::new();
    let mut group = c.benchmark_group("to_hub");
    for (name, spoke, build) in SPOKES {
        for &lines in &SIZES {
            let bytes = build(lines);
            group.throughput(Throughput::Bytes(bytes.len() as u64));
            group.bench_with_input(BenchmarkId::new(name, lines), &bytes, |b, bytes| {
                b.iter(|| engine.to_hub(spoke, black_box(bytes)).expect("well-formed"));
            });
        }
    }
    group.finish();
}

fn bench_from_hub(c: &mut Criterion) {
    let engine = Engine::new();
    let mut group = c.benchmark_group("from_hub");
    for (name, spoke, build) in SPOKES {
        for &lines in &SIZES {
            let hub = hub_of(&engine, spoke, &build(lines));
            group.bench_with_input(BenchmarkId::new(name, lines), &hub, |b, hub| {
                // `from_hub` consumes the hub, so each iteration gets a fresh
                // clone built outside the timed routine.
                b.iter_batched(
                    || hub.clone(),
                    |hub| engine.from_hub(spoke, black_box(hub)).expect("renderable"),
                    criterion::BatchSize::SmallInput,
                );
            });
        }
    }
    group.finish();
}

fn bench_transform(c: &mut Criterion) {
    let engine = Engine::new();
    let mut group = c.benchmark_group("transform");
    // Cartesian product of source x target spokes: same-spoke round-trips, the
    // reverse direction, and the cross-format routes.
    for (from_name, from, build) in SPOKES {
        for (to_name, to, _) in SPOKES {
            for &lines in &SIZES {
                let bytes = build(lines);
                let route = format!("{from_name}_to_{to_name}");
                group.throughput(Throughput::Bytes(bytes.len() as u64));
                group.bench_with_input(BenchmarkId::new(route, lines), &bytes, |b, bytes| {
                    b.iter(|| {
                        engine
                            .transform(from, to, black_box(bytes))
                            .expect("well-formed")
                    });
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_to_hub, bench_from_hub, bench_transform);
criterion_main!(benches);
