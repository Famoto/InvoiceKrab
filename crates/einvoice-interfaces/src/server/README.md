# server

## Purpose

The `server` module is the HTTP surface for the engine: it turns
`POST /transform?to=<format>[&from=<format>]` requests carrying source XML
into transformed XML, concurrently, with a hard upper bound on the memory
request traffic can consume. Listener/runtime/signal wiring lives in the
thin `src/bin/krab-server.rs`; everything decidable lives here, unit-tested
— including the full HTTP surface, driven in-process through the axum
router.

## Structure

- `config.rs` — `Config`: environment knobs with hardware defaults.
- `gate.rs` — `MemGate`/`Guard`: the global byte-budget admission gate
  (async; guards own the gate so they can outlive the handler).
- `handle.rs` — `handle()`: query + body → `Reply` (status, body, warnings).
- `router.rs` — `router()`: the axum `Router` wiring routes, per-frame body
  timeouts, and the gate to the handlers.
- `mod.rs` — module docs and the public re-exports.

## Configuration

| Variable                 | Default                                       |
|--------------------------|-----------------------------------------------|
| `KRAB_ADDR`              | `0.0.0.0:8080`                                |
| `KRAB_WORKERS`           | available parallelism (cgroup-aware)          |
| `KRAB_MEM_BUDGET_BYTES`  | detected memory x 1/2 (cgroup v2 limit first) |
| `KRAB_MEM_BLOWUP`        | `12` — reservation = Content-Length x blowup  |
| `KRAB_BODY_TIMEOUT_SECS` | `30` — per-frame body read/write timeout      |
| `KRAB_REQUEST_TIMEOUT_SECS` | `300` — deadline for the whole request body |
| `KRAB_QUEUE_TIMEOUT_SECS` | `60` — longest wait for a memory reservation |

Malformed values are startup errors, never silent fallbacks. `KRAB_WORKERS`
sets the tokio runtime worker threads and caps the blocking pool that runs
the CPU-bound transforms, so it bounds transform parallelism.

## Endpoints

| Route                                      | Response                                      |
|--------------------------------------------|-----------------------------------------------|
| `POST /transform?to=<f>[&from=<f>]`        | transformed XML (see denials below)           |
| `GET /formats`                             | JSON array of accepted format names           |
| `GET /analyze[?from=<f>[&to=<f>]][&deny_lossy=1]` | text loss/error matrix, row or pair report (the CLI's `--analyze`); `422` + report when `deny_lossy` and not lossless |
| `GET /health`                              | `200 ok` — the engine is stateless, alive = healthy |

`krab-server --healthcheck` probes `GET /health` on loopback and exits 0/1 —
the Docker `HEALTHCHECK` for the `FROM scratch` image, where no curl exists.

## Behavior

There is deliberately **no per-document size limit**. Memory safety comes
from admission control instead: before reading its body, a request reserves
`Content-Length x KRAB_MEM_BLOWUP` bytes (the measured peak of body + typed
model + hub + output) from a global budget. The default 12 covers the
worst measured route: for bodies of 1 MB and up, a FatturaPA source written
as Factur-X peaks at 11.5x the body under glibc and 9.0x under musl. Every
other source stays at or below 8.8x at any measured size, so
`KRAB_MEM_BLOWUP=9` is safe when FatturaPA is never an input (see
[docs/PERFORMANCE.md](../../../../docs/PERFORMANCE.md#memory)). Requests run
in parallel while budget remains; when it is exhausted, requests queue FIFO until a
reservation is released, for at most `KRAB_QUEUE_TIMEOUT_SECS`; after that
the request is shed with `503` and `Retry-After`, instead of holding a
connection whose client has likely given up. The reservation is held until the response is
*written* (it rides inside the response body), so a slow-reading client
cannot accumulate unreserved output. The process therefore cannot be driven
past its budget — no OOM from traffic.

Slow peers are bounded three ways: a per-frame body timeout
(`KRAB_BODY_TIMEOUT_SECS`, both directions) drops live-but-silent
connections; a whole-body deadline (`KRAB_REQUEST_TIMEOUT_SECS`, `408`) drops
clients that trickle bytes just inside the frame timeout, which would
otherwise hold their reservation (up to the whole budget) indefinitely; and
TCP keepalive on the listener reaps dead ones.

`SIGTERM`/`SIGINT` trigger a graceful shutdown: the listener stops
accepting, in-flight requests drain to completion, then the process exits 0.
The orchestrator's kill grace period is the drain deadline.

Denials:

| Condition                            | Response                     |
|--------------------------------------|------------------------------|
| unknown route                        | 404 + usage text             |
| no Content-Length (chunked upload)   | 411 + `Connection: close`    |
| reservation exceeds the whole budget | 413 + `Connection: close`    |
| no reservation within queue timeout  | 503 + `Retry-After` + close  |
| body not complete by request timeout | 408 + `Connection: close`    |
| body read fails or a frame times out | 400 + `Connection: close`    |
| missing/unknown/repeated format, bad XML, unsupported encoding | 400 |
| error-severity mapping diagnostics   | 422 with rendered diagnostics|
| unserializable output (engine bug)   | 500                          |

Warning-severity diagnostics on success are returned in the
`X-Krab-Warnings` response header; the 200 body is pure XML.

Query values are `application/x-www-form-urlencoded`-decoded, so clients may
percent-encode the `:` of versioned format names (`to=xrechnung-invoice%3A3.0.2`).

`krab-server --healthcheck` probes the bound address from `KRAB_ADDR`, or its
loopback (`127.0.0.1` / `[::1]`) when bound to all interfaces.

Known ceiling: connections are accepted eagerly (tokio), so once the budget
is exhausted the queue of gate waiters lives in userspace, one small task +
connection each; the queue timeout bounds how long each waits, not how many
there are. Header-only floods are cheap but not free; front
with nginx/caddy for hostile internet exposure.

## Testing

- `gate.rs` tests: immediate admission, RAII release, `NeverFits`,
  cross-task waiting and resumption, parallel admission, cancellation.
- `config.rs` tests: defaults, every override, malformed/zero rejection, the
  cgroup `"max"` sentinel, `/proc/meminfo` parsing, the healthcheck probe
  address — all against pure
  lookups/fixtures, never the process environment.
- `handle.rs` tests: every row of the status table plus source
  auto-detection.
- `router.rs` tests: the full HTTP surface via `tower::ServiceExt::oneshot`
  — every transport denial, headers, stalled- and trickling-upload timeouts
  and the queue timeout (paused time), and reservation-release timing. No sockets.
- Only the binary's listener/runtime/signal wiring is untested I/O. Smoke
  test:

```sh
cargo run --release -p einvoice-interfaces --bin krab-server &
curl -sS --data-binary @invoice.xml 'localhost:8080/transform?to=xrechnung-invoice'
curl -sS -X POST -H 'Transfer-Encoding: chunked' --data-binary @invoice.xml \
    'localhost:8080/transform?to=ubl-invoice'   # → 411
KRAB_MEM_BUDGET_BYTES=100 cargo run -p einvoice-interfaces --bin krab-server  # any request → 413
kill -TERM %1   # → "draining connections", exit 0
```

## Capacity check

HTTP throughput is machine- and network-bound, so it is measured manually per
deployment target (an in-repo bench would mostly measure loopback noise):

```sh
oha -z 15s -c 32 -m POST -D invoice.xml \
    'http://localhost:8080/transform?to=ubl-invoice&from=ubl-invoice'
```

Reference points in this file predate the axum/tokio port (tiny_http, 16-core
dev machine, loopback, 32 connections): ~219k req/s `GET /health`, ~148k
req/s for a 470 B invoice, ~17k req/s at 23 KB, ~1.7k req/s at 231 KB — all
data-plane CPU-bound, peak RSS well under 70 MB. Re-measure on the new stack
before quoting numbers. Under a deliberately starved budget the gate queues
instead of failing: throughput degrades, latency rises, every response stays
200, and RSS stays bounded.

Note on RSS readings: glibc retains freed arena memory, so resident memory
settles near the high-water mark of roughly one generation of concurrent
requests — bounded by the gate, but it does not shrink back to idle after a
burst. `MALLOC_ARENA_MAX` (or a different allocator) tightens it if strict
RSS matters.
