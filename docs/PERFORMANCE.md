# Performance and scalability

What throughput, response times and resource needs to expect from
KrabInvoice, how it scales, and how to size a deployment. All numbers are
measured; the test setup is described [at the end](#how-this-was-measured).

## At a glance

- **Fast.** A typical invoice is converted in about **1 ms** of processing
  time. One CPU core handles roughly **4,400 typical invoices per second**
  (about 15 million per hour).
- **Scales with hardware.** Throughput grows almost linearly with CPU cores,
  and the service holds no state, so you can add instances behind a load
  balancer.
- **Large invoices.** There is no fixed size limit; the only bound is
  available memory ([details](#overload-behavior)). Invoices with 100,000 lines
  (89 MB) were converted in about 2 seconds.
- **Predictable memory.** An invoice needs roughly **3–6× its file size** in
  memory while it is converted, and up to about **11×** for the very compact
  FatturaPA format. The service is 3 MB at idle.
- **Safe under overload.** The service never runs out of memory because of
  traffic. When memory is fully booked, new invoices wait their turn instead
  of failing.
- **One known limitation.** While very large invoices are being converted,
  small ones can wait behind them ([details](#known-limitation-mixed-invoice-sizes)).

## Throughput

![Invoices per second by invoice size](perf/capacity.svg)

How many invoices the HTTP service converts per second on a 4-core server:

| Invoice | File size | To UBL / XRechnung | To Factur-X |
|---|---:|---:|---:|
| Typical (3 lines) | 9 KB | 12,100 /s | 10,100 /s |
| 100 lines | 95 KB | 1,790 /s | 1,560 /s |
| 1,000 lines | 0.9 MB | 216 /s | 179 /s |
| 10,000 lines | 8.9 MB | about 19 /s | about 16 /s |

Processing cost is proportional to file size: an invoice ten times larger
takes about ten times longer. Converting to FatturaPA is about 30–40% faster
than to UBL; Factur-X is about 10–20% slower.

### Capacity per CPU core

For planning. The first two rows were measured with one worker (one core);
the larger sizes are the 4-core results divided by four:

| Invoice | Per second | Per hour |
|---|---:|---:|
| Typical (9 KB) | ~4,400 | ~15 million |
| 100 lines (95 KB) | ~500 | ~1.8 million |
| 1,000 lines (0.9 MB) | ~50 | ~180,000 |
| 10,000 lines (8.9 MB) | ~5 | ~18,000 |

## Scalability

### More CPU cores

![Throughput grows with CPU cores](perf/scaling-cores.svg)

| CPU cores | Typical invoice (9 KB) | 100-line invoice (95 KB) |
|---|---:|---:|
| 1 | 4,409 /s (1.0×) | 510 /s (1.0×) |
| 2 | 8,798 /s (2.0×) | 1,047 /s (2.1×) |
| 3 | 12,555 /s (2.8×) | 1,507 /s (3.0×) |
| 4 | 11,683 /s (2.6×)\* | 1,840 /s (3.6×)\* |

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
| Typical | 9 KB | 4 ms | 6 ms | 4 ms |
| 100 lines | 95 KB | 6 ms | 6 ms | 5 ms |
| 1,000 lines | 0.9 MB | 22 ms | 23 ms | 15 ms |
| 10,000 lines | 8.9 MB | 0.18 s | 0.21 s | 0.13 s |
| 100,000 lines | 89 MB | 2.0 s | 2.2 s | 1.2 s |

For invoices under ~100 KB the time is mostly the ~3 ms the program takes
to start. Through the HTTP service, which is already running, a typical
invoice takes about 1 ms.

## Memory

![Memory needed for one invoice](perf/invoice-memory.svg)

| Invoice | File size | Peak memory |
|---|---:|---:|
| Up to 100 lines | ≤ 95 KB | 7 MB |
| 1,000 lines | 0.9 MB | 9–11 MB |
| 10,000 lines | 8.9 MB | 26–57 MB |
| 100,000 lines | 89 MB | 220–420 MB |

The invoice is held in memory in full during conversion. How much memory
that takes per byte depends mostly on the **input format**:

![Memory needed depends on the input format](perf/format-memory.svg)

| Input format | Peak memory per request (Docker image) | Linux build |
|---|---:|---:|
| FatturaPA | up to 9.0× file size | up to 11.5× |
| UBL, XRechnung, Peppol | up to 4.5× | up to 5.9× |
| Factur-X | up to 3.1× | up to 3.8× |

Worst output format for each input, invoices of 1 MB and up, single request.
FatturaPA stores the same invoice in far fewer bytes (13 MB where UBL needs
89 MB), so the converted result is many times larger than the upload.
Writing Factur-X needs the most memory of the output formats. The Docker image
(musl) uses noticeably less memory than the standard Linux (glibc) build.
Invoices under 1 MB show higher multiples, but only by a few MB in absolute
terms.

![Memory stays bounded under load](perf/concurrency-memory.svg)

The service converts as many invoices at the same time as it has workers
(one per CPU core by default). Additional invoices wait in line, so memory
grows only slowly once every worker is busy: eight large invoices sent at
once used 174 MB, against 136 MB for four.

### Overload behavior

Before it reads an invoice, the service books memory for it: **12× the
invoice's file size**, the worst measured case above rounded up, taken from
a fixed memory budget (by default, half of
the server's or container's memory). If the budget is fully booked, the
invoice waits until earlier ones finish. Nothing is rejected, and memory
can't be exhausted by traffic.

In a test with a deliberately small 300 MB budget and eight large invoices
sent at once, all invoices were converted successfully; they took longer
because they queued.

The only invoices rejected are those that could never fit: **larger than
1/12 of the memory budget**. These receive HTTP `413`. With the default
budget, that means invoices larger than 1/24 of the container's memory
(about 85 MB for a 2 GB container, 420 MB for 10 GB).

## Sizing a deployment

Two settings matter: CPU cores (throughput) and memory (the largest invoice
and how many large invoices run at once).

- **CPU:** divide your peak invoice rate by the per-core capacity above, and
  add headroom.
- **Memory:** with the default settings, plan **24 × the largest invoice
  size × the number of workers** so all workers can take large invoices at
  once. Less is fine: large invoices then queue instead of running in
  parallel. The container must have at least 24 × the largest invoice size.
  If you never receive FatturaPA invoices, set `KRAB_MEM_BLOWUP=9` and use
  18× instead of 24×.

| Scenario | Largest invoice | Suggested container | Expected capacity |
|---|---|---|---|
| Standard e-invoicing | up to 1 MB | 1–2 cores, 256 MB | thousands of typical invoices per second |
| Mixed, occasional large invoices | up to 20 MB | 4 cores, 2 GB | ~12,000 typical invoices/s; four 20 MB invoices in parallel |
| Bulk / very large documents | up to 100 MB | 4 cores, 10 GB (8 GB with `KRAB_MEM_BLOWUP=9`) | four 100 MB invoices in parallel, ~2–3 s each |

Defaults adapt to the container's CPU and memory limits automatically. To
override them, set the environment variables below; see the
[README](../README.md#the-krab-server-http-api) for details.

| Setting | Default | Change it when |
|---|---|---|
| `KRAB_WORKERS` | number of CPU cores | you want to reserve CPU for other processes |
| `KRAB_MEM_BUDGET_BYTES` | half of the available memory | the container runs other processes, or you want a fixed limit |
| `KRAB_MEM_BLOWUP` | `12` (memory booked per byte of invoice) | you never receive FatturaPA input: `9` covers every other measured format and allows a third more large invoices per GB. Don't go below 9. |

### Command line or HTTP service?

- **Command line (`krab-cli`)**: good for scripts, batch jobs and occasional
  conversions. Each run pays ~3 ms of start-up, which limits one process to
  about 250 typical invoices per second.
- **HTTP service (`krab-server`)**: for continuous or high-volume traffic. No
  start-up cost per invoice, all cores used, and bounded memory.

## Known limitation: mixed invoice sizes

![Large invoices delay small ones](perf/mixed-workload.svg)

| Typical invoice, 4-core server | Invoices per second | Median response | Slowest 1% |
|---|---:|---:|---:|
| Only typical invoices | 9,850 | 0.7 ms | 2.2 ms |
| While eight 8.9 MB invoices are being converted | 36 | 223 ms | 320 ms |

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
| Hardware | 4 vCPU, 15 GB RAM, Linux container |
| Build | optimized release build; memory per format also measured on the musl build used by the Docker image |
| Service settings | defaults (4 workers, memory budget half of RAM) unless stated |
| Test tool | [`oha`](https://github.com/hatoo/oha) HTTP load generator, 8–32 parallel connections, on the same machine (results are conservative) |
| Invoices | the OASIS UBL 2.1 example invoice (bundled as `testfiles/UBL-Invoice-2.1.xml` until it was removed for licensing reasons; it is in the git history) and copies with 100 to 100,000 invoice lines; the XRechnung sample gave the same picture. For memory per format, those invoices were converted into every supported format and each was sent back through every route |

Results depend on hardware and on how many fields your invoices use;
measure with your own invoices before final sizing:

```bash
cargo build --release -p einvoice-interfaces
./target/release/krab-server &
oha -z 10s -c 32 -m POST -D my-invoice.xml \
    'http://localhost:8080/transform?to=xrechnung-invoice'
```

The charts are generated from the numbers in [perf/charts.py](perf/charts.py).
