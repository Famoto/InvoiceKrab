# server

## Purpose

The `server` module is the HTTP surface for the engine: it turns
`POST /transform?to=<format>[&from=<format>]` requests carrying source XML
into transformed XML, concurrently, with a hard upper bound on the memory
request traffic can consume. Socket handling lives in the thin
`src/bin/krab-server.rs`; everything decidable lives here, unit-tested.

## Structure

- `config.rs` — `Config`: environment knobs with hardware defaults.
- `gate.rs` — `MemGate`/`Guard`: the global byte-budget admission gate.
- `handle.rs` — `handle()`: query + body → `Reply` (status, body, warnings).
- `mod.rs` — module docs and the public re-exports.

## Configuration

| Variable                | Default                                       |
|-------------------------|-----------------------------------------------|
| `KRAB_ADDR`             | `0.0.0.0:8080`                                |
| `KRAB_WORKERS`          | available parallelism (cgroup-aware)          |
| `KRAB_MEM_BUDGET_BYTES` | detected memory x 1/2 (cgroup v2 limit first) |
| `KRAB_MEM_BLOWUP`       | `5` — reservation = Content-Length x blowup   |

Malformed values are startup errors, never silent fallbacks.

## Endpoints

| Route                                      | Response                                      |
|--------------------------------------------|-----------------------------------------------|
| `POST /transform?to=<f>[&from=<f>]`        | transformed XML (see denials below)           |
| `GET /formats`                             | JSON array of accepted format names           |
| `GET /analyze[?from=<f>]`                  | text loss/error matrix (the CLI's `--analyze`)|
| `GET /health`                              | `200 ok` — the engine is stateless, alive = healthy |

`krab-server --healthcheck` probes `GET /health` on loopback and exits 0/1 —
the Docker `HEALTHCHECK` for the `FROM scratch` image, where no curl exists.

## Behavior

There is deliberately **no per-document size limit**. Memory safety comes
from admission control instead: before reading its body, a request reserves
`Content-Length x KRAB_MEM_BLOWUP` bytes (the measured peak of body + typed
model + hub + output) from a global budget. Requests run in parallel while
budget remains; when it is exhausted, workers block FIFO until a reservation
is released, and the kernel accept backlog queues everything else. The
process therefore cannot be driven past its budget — no OOM from traffic.

Denials:

| Condition                            | Response                     |
|--------------------------------------|------------------------------|
| not `POST /transform`                | 404                          |
| no Content-Length (chunked upload)   | 411 + `Connection: close`    |
| reservation exceeds the whole budget | 413 + `Connection: close`    |
| missing/unknown format, bad XML      | 400                          |
| error-severity mapping diagnostics   | 422 with rendered diagnostics|
| unserializable output (engine bug)   | 500                          |

Warning-severity diagnostics on success are returned in the
`X-Krab-Warnings` response header; the 200 body is pure XML.

Known ceiling: `tiny_http` exposes no socket read timeout, so a client
sending its body arbitrarily slowly pins a worker (slowloris). Fine on
trusted networks; front with nginx/caddy for internet exposure — they buffer
and drop slow clients before the request reaches this process.

## Testing

- `gate.rs` tests: immediate admission, RAII release, `NeverFits`,
  cross-thread blocking and resumption, parallel admission.
- `config.rs` tests: defaults, every override, malformed/zero rejection, the
  cgroup `"max"` sentinel, `/proc/meminfo` parsing — all against pure
  lookups/fixtures, never the process environment.
- `handle.rs` tests: every row of the status table plus source
  auto-detection.
- The socket loop is an untested I/O boundary. Smoke test:

```sh
cargo run --release -p einvoice-interfaces --bin krab-server &
curl -sS --data-binary @invoice.xml 'localhost:8080/transform?to=xrechnung-invoice'
curl -sS -X POST -H 'Transfer-Encoding: chunked' --data-binary @invoice.xml \
    'localhost:8080/transform?to=ubl-invoice'   # → 411
KRAB_MEM_BUDGET_BYTES=100 cargo run -p einvoice-interfaces --bin krab-server  # any request → 413
```

## Capacity check

HTTP throughput is machine- and network-bound, so it is measured manually per
deployment target (an in-repo bench would mostly measure loopback noise):

```sh
oha -z 15s -c 32 -m POST -D invoice.xml \
    'http://localhost:8080/transform?to=ubl-invoice&from=ubl-invoice'
```

Reference points (16-core dev machine, loopback, 32 connections): ~219k req/s
`GET /health` (HTTP stack ceiling), ~148k req/s for a 470 B invoice,
~17k req/s at 23 KB, ~1.7k req/s at 231 KB — all data-plane CPU-bound, peak
RSS well under 70 MB. Under a deliberately starved budget the gate queues
instead of failing: throughput degrades, latency rises, every response stays
200, and RSS stays bounded.

Note on RSS readings: glibc retains freed arena memory, so resident memory
settles near the high-water mark of roughly one generation of concurrent
requests — bounded by the gate, but it does not shrink back to idle after a
burst. `MALLOC_ARENA_MAX` (or a different allocator) tightens it if strict
RSS matters.
