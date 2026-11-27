# Performance and scalability

What throughput, response times and resource needs to expect from
KrabInvoice, how it scales, and how to size a deployment. All numbers are
measured; the test setup is described [at the end](#how-this-was-measured).

## At a glance

- **Fast.** A typical invoice is converted in about **0.2 ms** of
  processing time. One CPU core handles roughly **4,400 typical invoices per
  second** (about 16 million per hour).
- **Scales with hardware.** Throughput grows almost linearly with CPU cores
  (7.1× on 8 cores), and the service holds no state, so you can add
  instances behind a load balancer.
- **Large invoices.** There is no fixed size limit; the only bound is
  available memory ([details](#overload-behavior)). Invoices with 100,000 lines
  (152 MB) were converted in about 1.3 seconds.
- **Predictable memory.** An invoice needs roughly **1–3× its file size** in
  memory while it is converted, and up to about **5×** for the very compact
  FatturaPA format (4× in the Docker image). The service is 4 MB at idle.
- **Safe under overload.** The service never runs out of memory because of
  traffic. When memory is fully booked, new invoices wait their turn instead
  of failing.
- **One known limitation.** While very large invoices are being converted,
  small ones can wait behind them ([details](#known-limitation-mixed-invoice-sizes)).

## Throughput

![Invoices per second by invoice size](perf/capacity.svg)

How many invoices the HTTP service converts per second on 4 CPU cores, from
a UBL invoice:

| Invoice | File size | To UBL | To XRechnung | To Peppol | To Factur-X | To FatturaPA |
|---|---:|---:|---:|---:|---:|---:|
| Typical (3 lines) | 16 KB | 17,900 /s | 17,200 /s | 18,100 /s | 18,600 /s | 20,600 /s |
| 100 lines | 165 KB | 2,430 /s | 2,430 /s | 2,430 /s | 2,440 /s | 2,680 /s |
| 1,000 lines | 1.5 MB | 286 /s | 288 /s | 288 /s | 291 /s | 317 /s |
| 10,000 lines | 15 MB | 29 /s | 29 /s | 29 /s | 29 /s | 32 /s |

Processing cost grows with file size: from about 100 KB up, an invoice ten
times larger takes about ten times longer. Converting to FatturaPA is about
10% faster than to the other formats; the other four cost the same.

### Every format pair

Invoices per second for every source and target format, 4 CPU cores. Each
source document is the same invoice converted to that format, so the rows
compare the same content in each format:

| Typical invoice, from ↓ to → | UBL | XRechnung | Peppol | Factur-X | FatturaPA |
|---|---:|---:|---:|---:|---:|
| UBL (16 KB) | 17,900 | 17,200 | 18,100 | 18,600 | 20,600 |
| XRechnung (14 KB) | 20,000 | 19,800 | 19,900 | 20,300 | 25,200 |
| Peppol (14 KB) | 20,400 | 20,300 | 20,600 | 20,800 | 24,300 |
| Factur-X (17 KB) | 20,200 | 20,100 | 20,200 | 20,700 | 24,300 |
| FatturaPA (4 KB) | 46,300 | refused\* | refused\* | 48,000 | 48,700 |

| 1,000 lines, from ↓ to → | UBL | XRechnung | Peppol | Factur-X | FatturaPA |
|---|---:|---:|---:|---:|---:|
| UBL (1.5 MB) | 286 | 288 | 288 | 291 | 317 |
| XRechnung (1.3 MB) | 328 | 328 | 325 | 327 | 365 |
| Peppol (1.3 MB) | 325 | 323 | 329 | 334 | 368 |
| Factur-X (1.9 MB) | 308 | 309 | 311 | 313 | 345 |
| FatturaPA (0.4 MB) | 722 | refused\* | refused\* | 711 | 747 |

The **source format** sets the cost; the target barely matters. Reading
UBL, XRechnung, Peppol or Factur-X costs about the same per invoice line.
FatturaPA input is 2–2.5× cheaper because it is a quarter of the size and,
in this test, carries fewer of the fields (the FatturaPA document was itself
converted from the UBL one). \*FatturaPA has no electronic addresses, buyer
reference or seller contact, which XRechnung and Peppol require, so these
routes are answered with `422` and the list of missing fields.

### Where the time goes

Engine time per invoice, without HTTP, measured with
`cargo bench -p einvoice-interfaces`:

| Format | Read, typical | Write, typical | Read, 100 lines | Write, 100 lines |
|---|---:|---:|---:|---:|
| UBL | 119 µs | 42 µs | 1.26 ms | 0.40 ms |
| XRechnung | 92 µs | 45 µs | 0.94 ms | 0.40 ms |
| Peppol | 96 µs | 42 µs | 1.01 ms | 0.41 ms |
| Factur-X | 93 µs | 39 µs | 0.94 ms | 0.36 ms |
| FatturaPA | 31 µs | 24 µs | 0.32 ms | 0.27 ms |

Reading the input is about three quarters of a conversion. Writes start
from the same invoice, read from UBL. Through the HTTP service a typical
UBL invoice costs 0.21 ms of server CPU against 0.17 ms in the engine, so
the HTTP layer adds about 0.04 ms per request.

### Capacity per CPU core

For planning, converting UBL to XRechnung with one worker on one core:

| Invoice | Per second | Per hour |
|---|---:|---:|
| Typical (16 KB) | ~4,400 | ~16 million |
| 100 lines (165 KB) | ~640 | ~2.3 million |
| 1,000 lines (1.5 MB) | ~75 | ~270,000 |
| 10,000 lines (15 MB) | ~7.5 | ~27,000 |

## Scalability

### More CPU cores

![Throughput grows with CPU cores](perf/scaling-cores.svg)

UBL to XRechnung, one worker per core:

| CPU cores | Typical invoice (16 KB) | 100-line invoice (165 KB) |
|---|---:|---:|
| 1 | 4,416 /s (1.0×) | 638 /s (1.0×) |
| 2 | 8,813 /s (2.0×) | 1,254 /s (2.0×) |
| 3 | 13,235 /s (3.0×) | 1,829 /s (2.9×) |
| 4 | 17,173 /s (3.9×) | 2,422 /s (3.8×) |
| 5 | 19,005 /s (4.3×)\* | 2,944 /s (4.6×) |
| 6 | 21,755 /s (4.9×)\* | 3,527 /s (5.5×) |
| 7 | 24,873 /s (5.6×)\* | 4,101 /s (6.4×) |
| 8 | 29,860 /s (6.8×)\* | 4,553 /s (7.1×) |

Each added core adds close to one core's worth of throughput. With the
100-line invoice the service kept every core busy at every step: 8 cores
deliver 7.1× one core, because each invoice costs about 11% more CPU time
when all cores run at once (lower all-core clock, shared cache). \*With the
typical invoice the test tool, which ran on the cores the service did not
use, could no longer keep the service busy from 5 cores up (about 80% busy).
Its CPU cost per invoice stayed at 0.22 ms throughout, so the typical
invoice scales like the 100-line one.

### More instances

The service keeps no state between requests, so instances are independent:
run as many as needed behind any HTTP load balancer. Total capacity is the
sum of the instances. This follows from the design; multi-instance
throughput was not measured separately.

### Larger invoices

![Time to convert one invoice](perf/invoice-time.svg)

Time to convert a single invoice, end to end, with the command-line tool:

| Invoice | File size | To UBL | To Factur-X | To FatturaPA |
|---|---:|---:|---:|---:|
| Typical | 16 KB | 1.4 ms | 1.4 ms | 1.3 ms |
| 100 lines | 165 KB | 2.9 ms | 2.9 ms | 2.7 ms |
| 1,000 lines | 1.5 MB | 16 ms | 15 ms | 14 ms |
| 10,000 lines | 15 MB | 0.14 s | 0.14 s | 0.13 s |
| 100,000 lines | 152 MB | 1.28 s | 1.25 s | 1.15 s |

XRechnung and Peppol take the same time as UBL (within 3%). For invoices
under ~100 KB the time is mostly the ~1.3 ms the program takes to start.
Through the HTTP service, which is already running, a typical invoice is
answered in under 1 ms.

### Faster XML output

Since November 2026 the XML of every output format is written by code
generated for each format, which writes the document in one pass instead of
copying it once per nesting level. The output is byte for byte the same.
Measured on the previous test machine (4 vCPU), the build before and after,
alternating rounds, with the OASIS example invoice (9 KB, and 0.9 MB with
1,000 lines):

| Invoice | Conversion | Before | After | Server CPU per invoice |
|---|---|---:|---:|---:|
| Typical (3 lines) | UBL → UBL | 13,200 /s | 15,600 /s | 0.26 → 0.22 ms (−18%) |
| | UBL → Factur-X | 12,500 /s | 15,300 /s | 0.28 → 0.22 ms (−22%) |
| 1,000 lines | UBL → UBL | 241 /s | 314 /s | 16.0 → 12.3 ms (−23%) |
| | UBL → Factur-X | 202 /s | 295 /s | 19.0 → 12.8 ms (−33%) |

Writing the XML itself became 5–10× faster; reading the input is now most of
a conversion's cost ([where the time goes](#where-the-time-goes)). Memory
drops where the output is large compared to the input
([memory per format](#memory)): FatturaPA → Factur-X peaked at 14.7× the
file size before and 9.3× after it.

## Memory

![Memory needed for one invoice](perf/invoice-memory.svg)

Peak memory of the command-line tool converting one UBL invoice:

| Invoice | File size | Peak memory |
|---|---:|---:|
| Up to 100 lines | ≤ 165 KB | 4.5–5 MB |
| 1,000 lines | 1.5 MB | 7–8 MB |
| 10,000 lines | 15 MB | 21–29 MB |
| 100,000 lines | 152 MB | 170–225 MB |

The upload is held in memory until it is parsed, and the converted
invoice until it is sent. In between, the invoice's values are kept in a
compact record (since November 2026: the values side by side, numbered by
the format's mapping, instead of one object per XML element), the
canonical hub keeps its invoice lines and other repeated items packed the
same way, and the upload is freed as soon as it is parsed. How much memory a conversion takes
per byte depends mostly on the **input format**:

![Memory needed depends on the input format](perf/format-memory.svg)

| Input format | Peak memory per request (Docker image) | Linux build |
|---|---:|---:|
| FatturaPA | up to 3.7× file size | up to 4.7× |
| UBL, XRechnung, Peppol | up to 2.3× | up to 3.2× |
| Factur-X | up to 2.2× | up to 2.5× |

Worst output format for each input, invoices of 1 MB and up, single request
on a fresh server, idle memory excluded, all 25 format pairs measured.
FatturaPA stores the same invoice in far fewer bytes (3.9 MB where UBL
needs 15 MB), so the converted result is many times larger than the upload;
Factur-X is the largest output, so FatturaPA → Factur-X is the worst route.
The Docker image (musl) uses noticeably less memory than the standard Linux
(glibc) build, whose allocator keeps more of the memory a conversion frees;
idle, the Docker image needs 3.9 MB and the Linux build 5.0 MB. Invoices
under 1 MB show higher multiples, but only by a few MB in absolute terms: a
0.4 MB FatturaPA invoice converted to Factur-X peaked at 6.9× (2.7 MB). For
an invoice of 10,000 lines (13–18 MB) the Docker image needs 1.1–1.7× the
file size, and 1.6–3.7× for the same invoice as FatturaPA (3.9 MB).

![Memory stays bounded under load](perf/concurrency-memory.svg)

The service converts as many invoices at the same time as it has workers
(one per CPU core by default). Additional invoices wait in line holding only
their upload, so once every worker is busy each waiting invoice adds about
its file size: with 4 workers, 15 MB invoices sent at once used 22 MB for
one, 83 MB for four, 142 MB for eight and 244 MB for sixteen. The memory
budget below caps the total.

### Overload behavior

Before it reads an invoice, the service books memory for it: **6× the
invoice's file size**, which covers the worst measured case above, taken from
a fixed memory budget (by default, half of
the server's or container's memory). If the budget is fully booked, the
invoice waits until earlier ones finish. Nothing is rejected, and memory
can't be exhausted by traffic.

In a test with a deliberately small 300 MB budget and eight 15 MB invoices
sent at once, the budget admitted three at a time. All eight were converted
successfully within 0.5 s, the later ones after waiting their turn, and the
service peaked at 140 MB.

The only invoices rejected are those that could never fit: **larger than
1/6 of the memory budget**. These receive HTTP `413`. With the default
budget, that means invoices larger than 1/12 of the container's memory
(about 170 MB for a 2 GB container, 850 MB for 10 GB).

## Sizing a deployment

Two settings matter: CPU cores (throughput) and memory (the largest invoice
and how many large invoices run at once).

- **CPU:** divide your peak invoice rate by the per-core capacity above, and
  add headroom.
- **Memory:** with the default settings, plan **12 × the largest invoice
  size × the number of workers** so all workers can take large invoices at
  once. Less is fine: large invoices then queue instead of running in
  parallel. The container must have at least 12 × the largest invoice size.
  If you never receive FatturaPA invoices, set `KRAB_MEM_BLOWUP=4` and use
  8× instead of 12×.

| Scenario | Largest invoice | Suggested container | Expected capacity |
|---|---|---|---|
| Standard e-invoicing | up to 1 MB | 1–2 cores, 256 MB | thousands of typical invoices per second |
| Mixed, occasional large invoices | up to 20 MB | 4 cores, 1 GB | ~17,000 typical invoices/s; four 20 MB invoices in parallel |
| Bulk / very large documents | up to 100 MB | 4 cores, 5 GB (3.5 GB with `KRAB_MEM_BLOWUP=4`) | four 100 MB invoices in parallel, ~1 s each |

Defaults adapt to the container's CPU and memory limits automatically. To
override them, set the environment variables below; see the
[README](../README.md#the-krab-server-http-api) for details.

| Setting | Default | Change it when |
|---|---|---|
| `KRAB_WORKERS` | number of CPU cores | you want to reserve CPU for other processes |
| `KRAB_MEM_BUDGET_BYTES` | half of the available memory | the container runs other processes, or you want a fixed limit |
| `KRAB_MEM_BLOWUP` | `6` (memory booked per byte of invoice) | you never receive FatturaPA input: `4` covers every other measured format and allows 1.5 times as many large invoices per GB. Don't go below 4. |

### Command line or HTTP service?

- **Command line (`krab-cli`)**: good for scripts, batch jobs and occasional
  conversions. Each run pays ~1.3 ms of start-up, which limits one process to
  about 700 typical invoices per second.
- **HTTP service (`krab-server`)**: for continuous or high-volume traffic. No
  start-up cost per invoice, all cores used, and bounded memory.

## Known limitation: mixed invoice sizes

![Large invoices delay small ones](perf/mixed-workload.svg)

| Typical invoice, 4 cores | Invoices per second | Median response | Slowest 1% |
|---|---:|---:|---:|
| Only typical invoices | 15,100 | 0.5 ms | 1.6 ms |
| While eight 15 MB invoices are being converted | 59 | 136 ms | 150 ms |

Each worker converts one invoice at a time, in arrival order. If every
worker is busy with a large invoice, small invoices wait until one finishes.
Health checks are not affected.

**Workaround:** if your traffic mixes very large invoices (several MB) with
time-sensitive small ones, route the large ones to a separate instance.
**Planned fix:** keep capacity reserved for small invoices
([#35](https://github.com/Famoto/InvoiceKrab/issues/35)).

## How this was measured

| | |
|---|---|
| Hardware | AMD Ryzen 7 3700X (8 cores, 16 threads), 31 GB RAM, Linux 7.3; a desktop machine with the default `powersave` governor and boost on, otherwise idle |
| Build | optimized release build (fat LTO) at commit `a1f82fc`, November 2026; memory per format also measured on the musl build used by the Docker image. The faster XML output comparison is older and was measured on a 4 vCPU / 15 GB Linux container |
| Service settings | defaults (memory budget half of RAM) unless stated; `KRAB_WORKERS` set to the number of cores. "4 cores" means the service pinned (`taskset`) to 4 physical cores with their second hardware threads left idle |
| Test tool | [`oha`](https://github.com/hatoo/oha) HTTP load generator on the same machine, pinned to the cores the service did not use (at 8 cores it shared them), 32 parallel connections (8 for 10,000 lines and for the mixed test), 6–10 s per point after a 1 s warm-up; core scaling is the median of three runs. Command-line times with [`hyperfine`](https://github.com/sharkdp/hyperfine) (median, at least 20 runs); engine times with `criterion` |
| Invoices | the EN 16931 example invoice (`testfiles/en16931-full-ubl.xml`, 16 KB, 3 lines) and copies with its lines repeated to 100, 1,000, 10,000 and 100,000 lines, each converted by `krab-cli` into the other four formats. Memory: one request on a fresh server, peak minus idle resident memory, every format pair at 1,000 and 10,000 lines |

All of it is repeatable with one script (Linux, needs `oha`, `hyperfine`
and `taskset`; raw results go to JSON files in the output directory):

```bash
cargo build --release -p einvoice-interfaces
cargo build --release --target x86_64-unknown-linux-musl -p einvoice-interfaces
python3 -I scripts/perf/measure.py ../krab-runs/perf      # about 25 minutes
cargo bench -p einvoice-interfaces                       # engine read/write times
```

Results depend on hardware and on how many fields your invoices use;
measure with your own invoices before final sizing:

```bash
cargo build --release -p einvoice-interfaces
./target/release/krab-server &
oha -z 10s -c 32 -m POST -D my-invoice.xml \
    'http://localhost:8080/transform?to=xrechnung-invoice'
```

The charts are generated from the numbers in [perf/charts.py](perf/charts.py).
