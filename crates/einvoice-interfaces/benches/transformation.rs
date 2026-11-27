//! Criterion benchmarks for the engine's transformation paths.
//!
//! Covers the three [`Engine`] entry points over **every** generated spoke
//! ([`Spoke::ALL`]) as both source and target:
//!
//! - `to_hub`    — source XML bytes → typed canonical hub (the read half).
//! - `from_hub`  — typed hub → target XML bytes (the write half).
//! - `transform` — the full N–1–N path (read + write through the hub), across
//!   the Cartesian product of source x target spokes, so same-spoke
//!   round-trips, the reverse direction, and the cross-format routes are all
//!   measured.
//!
//! Inputs are the EN 16931 example invoice (`testfiles/en16931-full-ubl.xml`,
//! 3 lines) and a copy with 100 lines, the fixtures `scripts/perf/measure.py`
//! builds; every other spoke's input is the engine's own conversion of the
//! UBL one. A regression in either the per-document or the per-line cost is
//! visible. Inputs are built once outside the timing loop; only the engine
//! call is measured. Run with `cargo bench -p einvoice-interfaces`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use einvoice_interfaces::{Engine, MainKey, Spoke};
use std::hint::black_box;
use std::time::Duration;

/// The EN 16931 example invoice, UBL syntax, three invoice lines.
const SEED: &str = include_str!("../../../testfiles/en16931-full-ubl.xml");

/// The two input sizes (invoice lines) exercised by every benchmark group.
const SIZES: [usize; 2] = [3, 100];

/// The example invoice with its three lines repeated, in turn, to `lines`
/// lines, each with a fresh line ID (BT-126).
fn ubl_invoice(lines: usize) -> Vec<u8> {
    const CLOSE: &str = "</cac:InvoiceLine>\n";
    let first = SEED.find("<cac:InvoiceLine>").expect("seed has lines");
    let start = SEED[..first].rfind('\n').expect("line starts a line") + 1;
    let end = SEED.rfind(CLOSE).expect("seed has lines") + CLOSE.len();
    let blocks: Vec<&str> = SEED[start..end].split_inclusive(CLOSE).collect();
    let mut xml = SEED[..start].to_owned();
    for i in 0..lines {
        let block = blocks[i % blocks.len()];
        let id = block.find("<cbc:ID>").expect("line has an ID") + "<cbc:ID>".len();
        let id_end = block.find("</cbc:ID>").expect("line has an ID");
        xml.push_str(&block[..id]);
        xml.push_str(&(i + 1).to_string());
        xml.push_str(&block[id_end..]);
    }
    xml.push_str(&SEED[end..]);
    xml.into_bytes()
}

/// The `lines`-line example invoice as a `spoke` document.
fn fixture(engine: &Engine, spoke: Spoke, lines: usize) -> Vec<u8> {
    let ubl = ubl_invoice(lines);
    if spoke == Spoke::UblInvoice {
        return ubl;
    }
    engine
        .transform(Spoke::UblInvoice, spoke, &ubl)
        .expect("example converts to every spoke")
        .value
        .expect("writer always yields a document")
        .into_bytes()
}

/// Short benchmark label: the format name without its version.
fn label(spoke: Spoke) -> &'static str {
    spoke.name().split(':').next().expect("split yields one part")
}

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
    for &spoke in Spoke::ALL {
        for lines in SIZES {
            let bytes = fixture(&engine, spoke, lines);
            group.throughput(Throughput::Bytes(bytes.len() as u64));
            group.bench_with_input(BenchmarkId::new(label(spoke), lines), &bytes, |b, bytes| {
                b.iter(|| engine.to_hub(spoke, black_box(bytes)).expect("well-formed"));
            });
        }
    }
    group.finish();
}

fn bench_from_hub(c: &mut Criterion) {
    let engine = Engine::new();
    let mut group = c.benchmark_group("from_hub");
    for &spoke in Spoke::ALL {
        for lines in SIZES {
            // The hub as read from UBL, the shared starting point of every
            // write.
            let hub = hub_of(&engine, Spoke::UblInvoice, &ubl_invoice(lines));
            group.bench_with_input(BenchmarkId::new(label(spoke), lines), &hub, |b, hub| {
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
    // 25 routes x 2 sizes: a shorter window keeps the group to minutes.
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));
    for &from in Spoke::ALL {
        for lines in SIZES {
            let bytes = fixture(&engine, from, lines);
            for &to in Spoke::ALL {
                let route = format!("{}_to_{}", label(from), label(to));
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
