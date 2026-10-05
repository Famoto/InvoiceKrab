# Performance

Measured speed, memory and scaling of `krab-cli` and `krab-server`, where the
CPU goes, and the open performance work. The charts are generated from the
tables below by [perf/charts.py](perf/charts.py) — re-run it after
re-measuring.

**Summary**

- Both the CLI and the server are CPU-bound and scale linearly with document
  size: about **40–75 MB/s per core** for the CLI and **110–190 MB/s** in
  total for the server on 4 cores.
- Below ~100 KB, the CLI's cost is process start-up (~3 ms, 7 MB RSS).
- Peak memory is **2.5–4.7× the input size** (the whole document, typed model,
  hub and output are held in memory). The server's 7× reservation covers it.
- ~90% of transform time is spent in **quick-xml's serde layer**. The
  generated mapping code is under 10%.
- The server's main weakness is **head-of-line blocking**: small requests wait
  hundreds of milliseconds behind large uploads ([#35]).

## Setup

| | |
|---|---|
| Build | `cargo build --release -p einvoice-interfaces` (fat LTO, 1 codegen unit) |
| Machine | 4 vCPU, 15 GB RAM, Linux container |
| Server | defaults: `KRAB_WORKERS=4`, 8.4 GB budget, `KRAB_MEM_BLOWUP=7` |
| Load | [`oha`](https://github.com/hatoo/oha), loopback, on the **same 4 cores** as the server — server numbers are a lower bound |
| Inputs | [testfiles/UBL-Invoice-2.1.xml](../testfiles/UBL-Invoice-2.1.xml) and [testfiles/xrechnung-3.0.2-beispiel.xml](../testfiles/xrechnung-3.0.2-beispiel.xml), plus copies with their `InvoiceLine`s repeated to 100, 1k, 10k and 100k lines |
| Profiler | `valgrind --tool=callgrind` on `krab-cli` |

## CLI (`krab-cli`)

![krab-cli wall time vs. input size](perf/cli-time.svg)

| Input (UBL) | → UBL | → Factur-X | → FatturaPA |
|---|---:|---:|---:|
| 9 KB (3 lines) | 4.4 ms | 6.1 ms | 4.1 ms |
| 95 KB (100 lines) | 5.9 ms | 6.3 ms | 4.9 ms |
| 0.9 MB (1k lines) | 21.9 ms | 22.5 ms | 15.5 ms |
| 8.9 MB (10k lines) | 175 ms | 210 ms | 125 ms |
| 89 MB (100k lines) | 1,993 ms | 2,185 ms | 1,220 ms |

Median of 5 runs, wall time including process start-up. `--help`, `--list`,
`--keys` and `--analyze` all finish in 3–7 ms. XRechnung input behaves the
same as UBL; it is ~25% smaller per line, so it is correspondingly faster.

![krab-cli peak memory vs. input size](perf/cli-memory.svg)

| Input (UBL) | → UBL | → Factur-X | → FatturaPA |
|---|---:|---:|---:|
| ≤ 95 KB | 7.4 MB | 7.4 MB | 7.4 MB |
| 0.9 MB | 9.0 MB | 10.8 MB | 7.3 MB |
| 8.9 MB | 39 MB | 57 MB | 26 MB |
| 89 MB | 340 MB (3.8×) | 419 MB (4.7×) | 219 MB (2.5×) |

There is no streaming: the CLI holds the input bytes, the typed source model,
the hub and the output string at once, so peak RSS grows linearly with input.

## Server (`krab-server`)

![krab-server POST /transform throughput](perf/server-throughput.svg)

| Endpoint (32 connections) | req/s | p50 | p99 |
|---|---:|---:|---:|
| `GET /health` | 77,000 | 0.35 ms | 1.4 ms |
| `GET /formats` | 75,650 | 0.36 ms | 1.5 ms |
| `GET /analyze` | 3,700 | 8.6 ms | 16.4 ms |
| `POST /transform` 9 KB → UBL | 12,144 | 2.6 ms | 5.6 ms |
| `POST /transform` 9 KB → Factur-X | 10,145 | 3.0 ms | 6.8 ms |
| `POST /transform` 95 KB → UBL | 1,787 | 17.7 ms | 26.0 ms |
| `POST /transform` 95 KB → Factur-X | 1,558 | 20.2 ms | 28.9 ms |
| `POST /transform` 0.9 MB → UBL | 216 | 146 ms | 193 ms |
| `POST /transform` 0.9 MB → Factur-X | 179 | 178 ms | 238 ms |

Transforms scale across cores (~4× the single-threaded CLI rate), and HTTP
overhead is small next to the transform work.

**Memory admission.** One 89 MB UBL → Factur-X request peaked at **354 MB**
server RSS (4× input; the server frees the request body after parsing, the
CLI does not). The reservation for it is 89 MB × 7 = 624 MB, so the default
`KRAB_MEM_BLOWUP=7` is safe with headroom. Eight concurrent 8.9 MB uploads
kept RSS at the same high-water mark.

**Graceful shutdown.** `SIGTERM` drains in-flight requests and exits 0.

### Head-of-line blocking

![Small requests stall behind large uploads](perf/server-head-of-line.svg)

| 9 KB UBL→UBL, 8 connections | req/s | p50 | p99 |
|---|---:|---:|---:|
| Alone | 9,850 | 0.70 ms | 2.17 ms |
| While 8 connections upload 8.9 MB documents | 36 | 223 ms | 320 ms |

Transforms run on a blocking pool capped at `KRAB_WORKERS`, first come first
served. The memory gate bounds bytes, not CPU slots, so it admits the large
uploads at once, and they occupy every transform thread. `GET /health` stays
fast (24k req/s) during the same test because it never uses that pool.
Tracked in [#35].

## Where the time goes

![Where transform time goes](perf/cpu-profile.svg)

| Component | UBL → UBL | UBL → Factur-X |
|---|---:|---:|
| quick-xml: parse XML into the typed source model | 56.9% | 49.9% |
| Generated reader (source model → hub) | 2.7% | 2.4% |
| quick-xml: serialize the target model, excl. name checks | 18.9% | — |
| quick-xml: element-name validation (`XmlName::try_from`) | 14.2% | 20.5% |
| Generated writer (hub → target model) | 6.9% | — |

Instruction shares from callgrind on a 1k-line (0.9 MB) invoice. For
Factur-X the writer total is 47.4%; the serializer and generated writer are
inlined together there, so only the name-validation share is separable.

What stands out:

- **The mapping logic is cheap.** The generated reader and writer together
  are under 10%; nearly all the time is the generic serde ↔ XML layer.
- **Element-name validation is pure overhead.** quick-xml checks every
  element name against the XML 1.1 name rules, character by character, each
  time it writes one. All names in generated code are compile-time
  constants, so the check never finds anything; it can't be switched off
  through the serde API.
- **Buffer growth.** About 10–13% of instructions are `Vec`/`String`
  reallocation, largely from the unsized output buffer in the generated
  `to_xml` ([#36]).

## Open performance work

| Issue | Change | Expected effect |
|---|---|---|
| [#35] | Keep a transform slot free for small requests (size-split lanes or weighted CPU admission) | Small-request latency stays near baseline under mixed load |
| [#36] | Pre-size the generated `to_xml` output buffer from the input length | Removes most of the ~10–13% realloc cost; less slack in peak memory |
| [#37] | Cache `GET /analyze` per `(from, to)` and keep CPU work off the async worker threads | `/analyze` at roughly `/formats` speed (~20×) |

Longer-term: since the code generator knows the full element tree, it could
emit direct readers and writers on quick-xml's event API instead of going
through serde. That removes the name validation and most of the generic
deserializer overhead. This is the largest remaining lever; based on the
profile, an estimated 2–3× (not measured).

Smaller notes:

- When `from=` is omitted, source detection makes up to two extra scans of
  the document. It stops at `CustomizationID`, so it's cheap for real
  invoices, but a UBL document without one gets a full extra pass.
- The criterion bench (`cargo bench -p einvoice-interfaces`) covers only
  synthetic 2- and 200-line invoices; the large-document behavior above
  isn't in it.

## Reproducing

```bash
cargo build --release -p einvoice-interfaces

# CLI
time ./target/release/krab-cli invoice.xml facturx-invoice > /dev/null

# Server
KRAB_ADDR=127.0.0.1:8080 ./target/release/krab-server &
oha -z 8s -c 32 -m POST -D invoice.xml \
    'http://127.0.0.1:8080/transform?to=ubl-invoice&from=ubl-invoice'
grep VmHWM /proc/$(pgrep krab-server)/status   # peak RSS

# Profile
valgrind --tool=callgrind ./target/release/krab-cli invoice.xml ubl-invoice > /dev/null
callgrind_annotate --inclusive=yes callgrind.out.*

# Charts (after updating the data tables in the script)
python3 docs/perf/charts.py
```

[#35]: https://github.com/Famoto/InvoiceKrab/issues/35
[#36]: https://github.com/Famoto/InvoiceKrab/issues/36
[#37]: https://github.com/Famoto/InvoiceKrab/issues/37
