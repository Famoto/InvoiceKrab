# Performance and scalability

What throughput, response times and resource needs to expect from
KrabInvoice, how it scales, and how to size a deployment. All numbers are
measured; the test setup is described [at the end](#how-this-was-measured).

## At a glance

- **Fast.** A typical invoice is converted in well under **1 ms** of
  processing time. One CPU core handles roughly **4,100 typical invoices per
  second** (about 15 million per hour).
- **Scales with hardware.** Throughput grows almost linearly with CPU cores,
  and the service holds no state, so you can add instances behind a load
  balancer.
- **Large invoices.** There is no fixed size limit; the only bound is
  available memory ([details](#overload-behavior)). Invoices with 100,000 lines
  (152 MB) were converted in about 1.5 seconds.
- **Predictable memory.** An invoice needs roughly **1–3× its file size** in
  memory while it is converted, and up to about **5×** for the very compact
  FatturaPA format (4× in the Docker image). The service is 3 MB at idle.
- **Safe under overload.** The service never runs out of memory because of
  traffic. When memory is fully booked, new invoices wait their turn instead
  of failing.
- **Small invoices stay fast beside large ones.** One worker is kept free
  for small invoices, so they are answered in about 3 ms while 15 MB
  invoices are converted ([details](#mixed-invoice-sizes)).

## Throughput

![Invoices per second by invoice size](perf/capacity.svg)

How many invoices the HTTP service converts per second on a 4-core server:

| Invoice | File size | To UBL / XRechnung | To Factur-X | To FatturaPA |
|---|---:|---:|---:|---:|
| Typical (3 lines) | 16 KB | 11,700 /s | 11,600 /s | 14,300 /s |
| 100 lines | 160 KB | 1,940 /s | 1,900 /s | 2,200 /s |
| 1,000 lines | 1.5 MB | 264 /s | 265 /s | 304 /s |
| 10,000 lines | 15 MB | about 25 /s | about 25 /s | about 28 /s |

Processing cost grows with file size: from about 100 KB up, an invoice ten
times larger takes about ten times longer. Converting to FatturaPA is about
10–20% faster than to the other formats; Factur-X costs the same as UBL.

### Capacity per CPU core

For planning, converting to UBL / XRechnung. The first two rows were
measured with one worker (one core); the larger sizes are the 4-core results
divided by four:

| Invoice | Per second | Per hour |
|---|---:|---:|
| Typical (16 KB) | ~4,100 | ~15 million |
| 100 lines (160 KB) | ~600 | ~2.2 million |
| 1,000 lines (1.5 MB) | ~65 | ~240,000 |
| 10,000 lines (15 MB) | ~6 | ~23,000 |

## Scalability

### More CPU cores

![Throughput grows with CPU cores](perf/scaling-cores.svg)

| CPU cores | Typical invoice (16 KB) | 100-line invoice (160 KB) |
|---|---:|---:|
| 1 | 4,076 /s (1.0×) | 606 /s (1.0×) |
| 2 | 8,528 /s (2.1×) | 1,115 /s (1.8×) |
| 3 | 12,582 /s (3.1×) | 1,778 /s (2.9×) |
| 4 | 10,949 /s (2.7×)\* | 1,947 /s (3.2×)\* |

Each added core adds close to one core's worth of throughput. \*The test
tool ran on the same 4-core machine and took CPU from the service at 4
cores, so the 4-core numbers understate real performance.

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
| Typical | 16 KB | 2.5 ms | 2.1 ms | 2.1 ms |
| 100 lines | 160 KB | 4.5 ms | 3.5 ms | 4.4 ms |
| 1,000 lines | 1.5 MB | 16 ms | 18 ms | 14 ms |
| 10,000 lines | 15 MB | 0.16 s | 0.15 s | 0.13 s |
| 100,000 lines | 152 MB | 1.4 s | 1.4 s | 1.1 s |

For invoices under ~100 KB the time is mostly the ~2 ms the program takes
to start. Through the HTTP service, which is already running, a typical
invoice is answered in under 1 ms.

### Faster XML output

Since November 2026 the XML of every output format is written by code
generated for each format, which writes the document in one pass instead of
copying it once per nesting level. The output is byte for byte the same.
Same 4-core server, the build before and after, alternating rounds, with the
OASIS example invoice (9 KB, and 0.9 MB with 1,000 lines):

| Invoice | Conversion | Before | After | Server CPU per invoice |
|---|---|---:|---:|---:|
| Typical (3 lines) | UBL → UBL | 13,200 /s | 15,600 /s | 0.26 → 0.22 ms (−18%) |
| | UBL → Factur-X | 12,500 /s | 15,300 /s | 0.28 → 0.22 ms (−22%) |
| 1,000 lines | UBL → UBL | 241 /s | 314 /s | 16.0 → 12.3 ms (−23%) |
| | UBL → Factur-X | 202 /s | 295 /s | 19.0 → 12.8 ms (−33%) |

Writing the XML itself became 5–10× faster; reading the input is now most of
a conversion's cost. Memory drops where the output is large compared to the input
([memory per format](#memory)): FatturaPA → Factur-X peaked at 14.7× the
file size before and 9.3× after it.

## Memory

![Memory needed for one invoice](perf/invoice-memory.svg)

| Invoice | File size | Peak memory |
|---|---:|---:|
| Up to 100 lines | ≤ 160 KB | 4–4.5 MB |
| 1,000 lines | 1.5 MB | 5–6.5 MB |
| 10,000 lines | 15 MB | 20–28 MB |
| 100,000 lines | 152 MB | 170–220 MB |

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
| FatturaPA | up to 4.2× file size | up to 4.9× |
| UBL, XRechnung, Peppol | up to 2.5× | up to 3.0× |
| Factur-X | up to 1.6× | up to 1.9× |

Worst output format for each input, invoices of 1 MB and up, single request,
idle memory excluded. FatturaPA stores the same invoice in far fewer bytes
(3.7 MB where UBL needs 15 MB), so the converted result is many times larger
than the upload; Factur-X is the largest output, so FatturaPA → Factur-X is
the worst route. The Docker image
(musl) uses noticeably less memory than the standard Linux (glibc) build,
whose allocator keeps more of the memory a conversion frees. Invoices under
1 MB show higher multiples, but only by a few MB in absolute terms. For an
invoice of 10,000 lines (12–18 MB) the Docker image needs 1.1–1.7× the file
size, and 1.4–3.7× for the same invoice as FatturaPA (3.7 MB).

![Memory stays bounded under load](perf/concurrency-memory.svg)

The service converts as many invoices at the same time as it has workers
(one per CPU core by default). Additional invoices wait in line holding only
their upload, so once every worker is busy each waiting invoice adds about
its file size: eight 15 MB invoices sent at once used 132 MB, against 71 MB
for four. The memory budget below caps the total.

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
service peaked at 135 MB.

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
| Mixed, occasional large invoices | up to 20 MB | 4 cores, 1 GB | ~12,000 typical invoices/s; four 20 MB invoices in parallel |
| Bulk / very large documents | up to 100 MB | 4 cores, 5 GB (3.5 GB with `KRAB_MEM_BLOWUP=4`) | four 100 MB invoices in parallel, ~1–2 s each |

Defaults adapt to the container's CPU and memory limits automatically. To
override them, set the environment variables below; see the
[README](../README.md#the-krab-server-http-api) for details.

| Setting | Default | Change it when |
|---|---|---|
| `KRAB_WORKERS` | number of CPU cores | you want to reserve CPU for other processes |
| `KRAB_MEM_BUDGET_BYTES` | half of the available memory | the container runs other processes, or you want a fixed limit |
| `KRAB_MEM_BLOWUP` | `6` (memory booked per byte of invoice) | you never receive FatturaPA input: `4` covers every other measured format and allows 1.5 times as many large invoices per GB. Don't go below 4. |
| `KRAB_SMALL_BYTES` | `262144` (256 KB: larger invoices leave one worker free) | you only convert large invoices in bulk: set it above your largest invoice so they use every worker |

### Command line or HTTP service?

- **Command line (`krab-cli`)**: good for scripts, batch jobs and occasional
  conversions. Each run pays ~2 ms of start-up, which limits one process to
  about 400 typical invoices per second.
- **HTTP service (`krab-server`)**: for continuous or high-volume traffic. No
  start-up cost per invoice, all cores used, and bounded memory.

## Mixed invoice sizes

![Small invoices beside large ones](perf/mixed-workload.svg)

| Typical invoice, 4-core server | Invoices per second | Median response | Slowest 1% |
|---|---:|---:|---:|
| Only typical invoices | 8,100 | 0.9 ms | 2.4 ms |
| While eight 15 MB invoices are being converted, before | 51 | 160 ms | 220 ms |
| The same, with a worker kept free for small invoices | 2,150 | 3.0 ms | 12 ms |

Each worker converts one invoice at a time. Invoices of 256 KB or more
(`KRAB_SMALL_BYTES`) take at most all workers but one, in arrival order, so
the last worker is always free for smaller invoices. Before, eight 15 MB
invoices took every worker and a typical invoice waited until one finished
([#35](https://github.com/Famoto/InvoiceKrab/issues/35)). Health checks were
never affected.

The price is one idle worker while only large invoices arrive: 15 MB
invoices alone ran at 18.6 per second instead of 24 (−23%, about one worker
in four). If you only convert large invoices in bulk, set
`KRAB_SMALL_BYTES` above your largest invoice so they use every worker.
With a single worker nothing is kept free.

## How this was measured

| | |
|---|---|
| Hardware | 4 vCPU, 15 GB RAM, Linux container |
| Build | optimized release build; memory per format also measured on the musl build used by the Docker image. The faster XML output was compared against the build before it in November 2026, alternating rounds. Throughput, times and memory were re-measured with the compact records and the packed hub in November 2026; memory per format is the peak RSS of one request on a fresh server, idle excluded |
| Service settings | defaults (4 workers, memory budget half of RAM) unless stated |
| Test tool | [`oha`](https://github.com/hatoo/oha) HTTP load generator, 8–32 parallel connections, on the same machine (results are conservative) |
| Invoices | the EN 16931 example invoice (`testfiles/en16931-full-ubl.xml`, 16 KB, 3 lines) and copies with 100 to 100,000 invoice lines, sent as UBL. The faster XML output comparison used the OASIS UBL 2.1 example (bundled as `testfiles/UBL-Invoice-2.1.xml` until it was removed for licensing reasons; it is in the git history), which lacks fields that XRechnung, Peppol and FatturaPA require. For memory per format, copies of the EN 16931 example (`testfiles/en16931-full-ubl.xml`) with 1,000 and 10,000 lines (2,800 for FatturaPA, to reach 1 MB) were converted into every supported format and each was sent through every route |

Results depend on hardware and on how many fields your invoices use;
measure with your own invoices before final sizing:

```bash
cargo build --release -p einvoice-interfaces
./target/release/krab-server &
oha -z 10s -c 32 -m POST -D my-invoice.xml \
    'http://localhost:8080/transform?to=xrechnung-invoice'
```

The charts are generated from the numbers in [perf/charts.py](perf/charts.py).
