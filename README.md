# InvoiceKrab

**KrabInvoice** transforms electronic-invoice XML documents from one format to
another (UBL, XRechnung, PEPPOL BIS Billing, CII, Factur-X/ZUGFeRD,
FatturaPA, ...) through a single shared canonical model — an **N → 1 → N**
transformation. Every format is described by a small **TOML mapping file**; the
build compiles those files into native Rust mappers, so the runtime never
interprets the TOML — it executes generated, type-checked code.

```
UBL / PEPPOL / XRechnung ─┐                  ┌─► UBL / PEPPOL / XRechnung
                          ├─► canonical hub ─┤
CII / Factur-X / FatturaPA┘      (MainKey)   └─► CII / Factur-X / FatturaPA
```

Adding a new format is just dropping a new `*.toml` into [config/mappings/](config/mappings/);
the lexical codecs formats share (CII's `format="102"` dates, …) live beside
them in [config/codecs/](config/codecs/).
No Rust code names any format by hand. A CIUS (a national/sector profile of a
base syntax) can *inherit* another mapping's whole tree and restate only its
deltas — XRechnung and Peppol are a handful of lines on top of UBL.

## Disclaimer

> [!WARNING]
> **The bundled mappings are demos, not a compliance product.**
>
> The mappings in [config/mappings/](config/mappings/) are **examples and
> starting points** for writing your own: they show how the DSL expresses
> real formats. They are **not certified, not complete, and not maintained
> against regulatory changes**. They pin values that are wrong for many senders
> (FatturaPA's tax regime and recipient code, for instance), they are checked
> against their XSDs but not against the official business rules (EN 16931,
> XRechnung or Peppol Schematron), and they cover only the document types and
> profiles listed below.
>
> KrabInvoice is a **framework**: you write the mappings for the formats,
> profiles and business rules you need, and compile them into your own build.
> No release binaries or container images are published, on purpose. Electronic
> invoices carry legal and tax obligations, and **you are responsible for
> validating every document you produce or accept** with the official tools
> of the networks and authorities you exchange invoices with. The software is
> provided without warranty of any kind; see [LICENSE](LICENSE) (AGPL-3.0,
> sections 15 and 16).

---

## Table of contents

- [Disclaimer](#disclaimer)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Bundled (demo) mappings](#bundled-demo-mappings)
- [The `krab-cli` CLI](#the-krab-cli-cli)
- [The `krab-server` HTTP API](#the-krab-server-http-api)
- [Library usage](#library-usage)
- [Features](#features)
- [The mapping DSL](#the-mapping-dsl)
- [Adding a new format](#adding-a-new-format)
- [Performance](#performance)
- [Workspace layout](#workspace-layout)
- [Developer commands](#developer-commands)

---

## Installation

KrabInvoice is a Rust workspace. You need a recent stable Rust toolchain
(edition 2024).

```bash
# Build the optimized CLI binary
cargo build --release -p einvoice-interfaces

# The binary lands at:
./target/release/krab-cli
```

Or run it straight from the workspace without installing:

```bash
cargo run --release -p einvoice-interfaces --bin krab-cli -- --help
```

The examples below assume `krab-cli` is on your `PATH`.

---

## Quick start

```bash
# Convert a UBL invoice to XRechnung (source format auto-detected)
krab-cli invoice.xml xrechnung-invoice --out invoice-xr.xml

# List every format this build knows about
krab-cli --list

# See, without any input document, which conversions lose data
krab-cli --analyze

# Inspect the canonical key vocabulary while writing mappings
krab-cli --keys
```

---

## Bundled (demo) mappings

These mappings are **demos and templates** (see the
[disclaimer](#disclaimer)): copy, adapt or replace them for your own formats.
The exact list is generated from [config/mappings/](config/mappings/) at build
time and can be checked with `krab-cli --list`. This workspace currently ships:

| Display name | Mapping file | Inherits | Notes |
|--------------|--------------|----------|-------|
| `ubl-invoice:2.1` | [config/mappings/ubl.toml](config/mappings/ubl.toml) | — | Base UBL Invoice tree (no `CreditNote`), covering the EN 16931 business terms; reads `CustomizationID` `urn:cen.eu:en16931:2017` |
| `xrechnung-invoice:3.0.2` | [config/mappings/xrechnung.toml](config/mappings/xrechnung.toml) | `ubl-invoice:2.1` | XRechnung CIUS, identified by its exact `CustomizationID` |
| `peppol-bis-billing:3.0` | [config/mappings/peppol.toml](config/mappings/peppol.toml) | `ubl-invoice:2.1` | Peppol BIS Billing CIUS, identified by its exact `CustomizationID` |
| `facturx-invoice:1.0` | [config/mappings/facturx.toml](config/mappings/facturx.toml) | `cii-invoice:en16931` | Factur-X / ZUGFeRD (EN 16931 and BASIC profiles), identified by its exact guideline id |
| `fatturapa:1.2.2` | [config/mappings/fatturapa.toml](config/mappings/fatturapa.toml) | — | Italian FatturaPA (`FatturaElettronica` tree, one invoice per file, transmission data pinned), identified by its `versione` attribute |

[config/mappings/cii.toml](config/mappings/cii.toml) carries the full UN/CEFACT CII tree but is
an **inherit-only base** (`[meta].disabled = true`): it exists to be inherited by
Factur-X/ZUGFeRD and emits no spoke of its own, so it does not appear in
`--list`.

To start your own set, replace or remove these files: the build compiles
whatever `config/mappings/*.toml` contains, and the demo spokes are not needed
by anything else. Keep [config/derivations.toml](config/derivations.toml) (the
EN 16931 calculation rules) and [config/codecs/](config/codecs/) as long as
your mappings use their keys and codecs.

---

## The `krab-cli` CLI

```
USAGE:
    krab-cli <INPUT> <TARGET-FORMAT> [--from <SOURCE-FORMAT>] [--out <FILE>]
    krab-cli --analyze [SOURCE-FORMAT [TARGET-FORMAT]] [--deny-lossy]
    krab-cli --keys [FORMAT]
    krab-cli --check [ROOT]
    krab-cli --list
    krab-cli --help

ARGS:
    <INPUT>            Source XML file, or `-` to read stdin
    <TARGET-FORMAT>    Format to emit (see --list)

OPTIONS:
    --from <FORMAT>    Source format; auto-detected when omitted
    --out <FILE>       Write to FILE instead of stdout
    --analyze          Report transforms' loss/error state: the whole matrix,
                       one source's row, or one SOURCE TARGET pair in full
    --to <FORMAT>      With --analyze: the target format of the pair
    --deny-lossy       With --analyze: exit 65 unless every reported
                       transform is lossless (a CI gate)
    --keys [FORMAT]    Show canonical main keys; with FORMAT, show that
                       spoke's covered and unused keys
    --check [ROOT]     Run the schema-conformance checks the mappings declare
                       (sample and output XSD validity, round trips) on the
                       files under ROOT (default: .); exit 65 on a failure
    --list             List available formats
    -h, --help         Show this help
```

### Transform a document

```bash
# File in, file out, source auto-detected
krab-cli in.xml ubl-invoice --out out.xml

# Pin the source format explicitly (skips auto-detection)
krab-cli in.xml xrechnung-invoice --from ubl-invoice

# Pipe through stdin/stdout (use `-` for the input)
cat in.xml | krab-cli - ubl-invoice > out.xml
```

Format names are **case-insensitive** and accept either the full versioned
display name (`ubl-invoice:2.1`) or the bare prefix (`ubl-invoice`).

Input may be in any of UTF-8, UTF-16, ISO-8859-1, windows-1252 or US-ASCII,
as the document's BOM or XML declaration says; it is transcoded to UTF-8
before it is read. Output is always UTF-8. Any other declared encoding is an
error (exit 65).

The transformed XML is written to stdout (or `--out`). Mapping **diagnostics**
(warnings, info, errors) are written to **stderr**, so they never corrupt the
output stream. If the mapping produces any *error*-severity diagnostic, no
partial output is emitted and the process exits non-zero.

### List formats

```bash
krab-cli --list
```

Prints every format compiled into this build (one per `config/mappings/*.toml`).

### Analyze conversions (no input needed)

`--analyze` statically reports the loss/error state of conversions — which
target formats can represent everything a source carries, which would drop
fields, and which cannot be fed a value they require — *without* needing an
actual document. It compares the two formats' transformation contracts (what
each maps, what each requires on write, what each declares it may lose).

```bash
# Full source x target matrix
krab-cli --analyze

# Scope to "from UBL to everything else"
krab-cli --analyze ubl-invoice

# One pair in full: missing required routes, dropped keys, pins, recodes,
# and what the source collapses on read
krab-cli --analyze ubl-invoice facturx-invoice

# A CI gate: exit 65 unless the pair is lossless
krab-cli --analyze ubl-invoice xrechnung-invoice --deny-lossy
```

### Inspect canonical keys (authoring aid)

`--keys` reports the canonical hub vocabulary without parsing an XML document.
With no format it lists every main key, which spokes define it, and which spokes
require it. With a format it shows the keys that spoke already maps and the hub
keys it does not yet cover.

```bash
# Whole canonical vocabulary
krab-cli --keys

# Covered vs. unused keys for one mapping
krab-cli --keys xrechnung-invoice
```

### Check schema conformance

`--check` runs the checks the mappings declare in `[meta.schema]` and
`[[meta.samples]]`: every sample validates against the XSD of the format that
reads it, and written by every format with a schema it validates against that
format's XSD (up to its documented `known_gaps`) and reads back with the same
value for every canonical key the format covers. Keys a format does not cover
are reported, not failed; so are the samples a format is documented to refuse
because it cannot represent their data (`[meta.schema].refuses`). The declared paths are relative to ROOT, the
workspace root (default: the current directory).

```bash
# From the repository root
krab-cli --check
```

Schema validation needs `xmllint` (libxml2) on `PATH`; without it the schema
checks are skipped with a notice and the round trips still run. A failed check
exits 65 with the report on stderr. See
[Schema conformance](config/mappings/README.md#schema-conformance) in the DSL
reference.

### Exit codes

KrabInvoice follows BSD `sysexits.h` conventions:

| Code | Meaning |
|------|---------|
| `0`  | Success (warnings/info may still appear on stderr) |
| `64` | Usage error — bad arguments, unknown format, or ambiguous source |
| `65` | Data error — input couldn't be parsed/rendered, mapping had errors, or a gate failed (`--analyze --deny-lossy`, `--check`) |
| `74` | I/O error — couldn't read input or write output |

---

## The `krab-server` HTTP API

The same transformation as an HTTP service, one document per request,
processed concurrently across a worker pool:

```bash
cargo run --release -p einvoice-interfaces --bin krab-server
# krab-server listening on 0.0.0.0:8080 — 16 workers, ... bytes memory budget, x12 reservation,
#   30s body timeout, 300s request timeout, 60s queue timeout

curl -sS --data-binary @invoice.xml \
    'localhost:8080/transform?to=xrechnung-invoice&from=ubl-invoice'
```

`POST /transform?to=<format>[&from=<format>]` — body is the source XML;
`from` is auto-detected when omitted. Query values are URL-decoded, so
`to=xrechnung-invoice%3A3.0.2` works like `to=xrechnung-invoice:3.0.2`; a
parameter given twice is a `400`. `200` returns the transformed XML
(warning diagnostics in the `X-Krab-Warnings` header), `400` bad or repeated
parameters, malformed XML or an unsupported character encoding, `422` mapping
errors (rendered diagnostics in the body), `411` missing Content-Length,
`413` a request that could never fit the memory budget, `408` a body not
received within the request timeout, `503` (with `Retry-After`) a request that
waited longer than the queue timeout for memory.

Capability and health endpoints: `GET /formats` (JSON array of accepted
format names), `GET /analyze[?from=<format>[&to=<format>]][&deny_lossy=1]`
(the CLI's `--analyze` report; `deny_lossy` answers `422` with the report when
the result is not lossless), `GET /health` (`200 ok`; `krab-server --healthcheck` self-probes it for the
Docker `HEALTHCHECK`, on the bound address, or on loopback when bound to
`0.0.0.0` / `[::]`).

`krab-server` has **no authentication and no TLS**: run it in a private
network or behind a reverse proxy that authenticates clients and terminates
TLS.

Configuration is environment variables; defaults derive from the actual
hardware (cgroup-aware, so container limits are respected):

| Variable                | Default                                       |
|-------------------------|-----------------------------------------------|
| `KRAB_ADDR`             | `0.0.0.0:8080`                                |
| `KRAB_WORKERS`          | available parallelism                         |
| `KRAB_MEM_BUDGET_BYTES` | detected memory x 1/2 (cgroup v2 limit first) |
| `KRAB_MEM_BLOWUP`       | `12` — reservation = Content-Length x blowup  |
| `KRAB_BODY_TIMEOUT_SECS` | `30` — longest gap between body frames       |
| `KRAB_REQUEST_TIMEOUT_SECS` | `300` — deadline for the whole request body |
| `KRAB_QUEUE_TIMEOUT_SECS` | `60` — longest wait for a memory reservation |

There is no per-document size limit. Instead, each request reserves
`Content-Length x KRAB_MEM_BLOWUP` bytes from a global budget before its
body is read; requests run in parallel while budget remains and queue when
it is exhausted, so request traffic can never drive the process out of
memory. A queued request is shed with `503` after `KRAB_QUEUE_TIMEOUT_SECS`,
and an admitted one must deliver its whole body within
`KRAB_REQUEST_TIMEOUT_SECS`, so a slow or stalled client cannot hold the
budget indefinitely. The default blowup of 12 is the worst measured peak (a FatturaPA
source written as Factur-X) rounded up; `KRAB_MEM_BLOWUP=9` is safe when
FatturaPA is never an input. A request larger than budget / blowup is
refused with `413` — with the defaults, about 1/24 of the memory limit (e.g.
~85 MB for a 2 GB container). See
[crates/einvoice-interfaces/src/server/README.md](crates/einvoice-interfaces/src/server/README.md)
and the measurements in [docs/PERFORMANCE.md](docs/PERFORMANCE.md#memory).

The Dockerfile ships both programs: `docker build --target server` for the
HTTP service (default), `--target cli` for the CLI image. All knobs are
runtime environment variables — set them per container, never at build time:

```bash
docker build --target server -t krab-server .

# Defaults derive from the container's own limits: workers from --cpus,
# memory budget = half of --memory (cgroup v2).
docker run --rm -p 8080:8080 --cpus 4 --memory 2g krab-server

# Explicit overrides win over detection (blowup 9: no FatturaPA input).
docker run --rm -p 8080:8080 \
    -e KRAB_WORKERS=8 \
    -e KRAB_MEM_BUDGET_BYTES=1000000000 \
    -e KRAB_MEM_BLOWUP=9 \
    krab-server
```

---

## Library usage

The CLI is a thin shell over `einvoice_interfaces::Engine`, which callers can use
directly:

```rust
use einvoice_interfaces::{Engine, Spoke};

let engine = Engine::new();
let result = engine.transform(Spoke::UblInvoice, Spoke::XrechnungInvoice, xml_bytes)?;
for diag in &result.diagnostics {
    eprintln!("{diag:?}"); // structured, with severity + source node
}
let xml: Option<String> = result.value;
```

`to_hub` (source bytes → typed `MainKey` hub) and `from_hub` (`MainKey` → target
XML) expose the two halves separately. Mapping-level problems (missing required
fields, type errors, taken fallbacks) are *diagnostics* in the `MappingResult`;
an `EngineError` only means the XML could not be parsed or rendered at all.

---

## Features

- **N → 1 → N transformation.** Every format maps to and from one shared
  canonical model, so adding a format makes it interoperable with *all* the
  others — no per-pair conversion code.
- **Shared lexical codecs.** Date, date-time and boolean wire forms are
  declared once in `config/codecs/` with a tiny compiler-checked pattern
  language (`YYYYMMDD` plus `format="102"`) and reused by every mapping.
- **Declarative TOML mappings.** A format is described by data, not code. The
  node ids mirror the XML element tree, so you describe *what* maps where, never
  *how* to walk the document. One element reused for several business terms
  (CII's `AdditionalReferencedDocument` by `TypeCode`) is split with a `match`
  selector, read by partition and written back with its discriminators.
- **Mapping inheritance.** A CIUS spoke inherits its base syntax's whole tree
  and restates only its deltas. A base can be inherit-only
  (`[meta].disabled = true`) so it never emits a spoke itself.
- **Compile-time safety.** The mapping compiler fails the build on unknown keys,
  fallback cycles, missing types, and cross-format type conflicts. If it builds,
  the mappers are type-checked Rust.
- **No runtime interpretation.** TOML is compiled to native Rust mappers at
  build time; at run time the engine only executes generated code.
- **Generated format registry.** The build scans `config/mappings/*.toml` and derives
  the public `Spoke` enum, module names, display names, and document identities.
- **Strict document identity.** Every read — format auto-detected or named with
  `--from` — first checks that the document is one of the source format: its
  root element's namespace URI and local name, its exact profile identifier
  (`CustomizationID` / guideline id) against the format's whitelist, a
  supported version, and mandatory identity attributes. A document that merely
  looks like the format is refused, not read.
- **Source auto-detection.** Omit `--from` and KrabInvoice picks the one format
  whose identity the document has; the build guarantees at most one matches.
- **Diagnostics, not silent loss.** Missing required fields, type errors, and
  taken fallbacks are reported as structured diagnostics with severity and a
  source-node reference — they don't vanish.
- **Calculation rules derive and check.** EN 16931 totals a source lacks are
  derived by their calculation rules (`VALUE_DERIVED`); totals it carries are
  recomputed, and a contradiction (a wrong amount due, a line sum that does
  not match the lines) is reported as `VALUE_INCONSISTENT`, a warning or an
  error as [config/derivations.toml](config/derivations.toml) declares.
- **Character encodings.** UTF-8, UTF-16, ISO-8859-1, windows-1252 and
  US-ASCII sources are read (transcoded by BOM or XML declaration); output is
  UTF-8.
- **Static conversion analysis.** Every format carries a transformation
  contract; `--analyze` compares two to show what a conversion would lose or
  fail to fill before you run it, and `--deny-lossy` gates CI on it.
- **Canonical key authoring aid.** `--keys` shows the hub vocabulary and, for one
  format, which existing keys are still unmapped.
- **Declared schema conformance.** A mapping names the XSD its format is
  defined by and the sample documents that prove it; the checks (sample
  validity, output validity, round trips) are derived from those
  declarations, run in `cargo test`, and on demand with `--check`.
- **Namespace-agnostic reading, namespaced writing.** Mappings bind XML *local*
  names, so the same mapping reads real namespaced (`cbc:`/`cac:`) UBL and
  bare-name fixtures; on write the declared namespaces are emitted on the root
  and every element is qualified (`cbc:`/`cac:`, `rsm:`/`ram:`/`udt:`, …).
- **Schema-ordered output.** A mapping's declaration order is its schema order:
  the writer emits sibling elements in the sequence the mapping declares them,
  so the bundled mappings follow their XSDs and the output validates in order.
- **Library API.** `einvoice-interfaces::Engine` exposes `to_hub`, `from_hub`,
  and `transform` for callers that want the generated mappers without the CLI.

---

## The mapping DSL

A mapping file (a "spoke") describes one document format in declarative TOML.
The big idea: each node's dotted table id *is* its XML element path — there is
no separate source spec and no hand-written path. At build time each spoke
compiles to a typed Rust source struct, `read`/`write` mappers, and the
format's contribution to the shared canonical hub. Anything wrong — unknown
fields, type conflicts, fallback cycles, invalid constants — **fails the
build**, never the runtime.

```toml
[Invoice.ID]                    # <Invoice><ID> — the id is the XML path
type = "identifier"
canonical_key = "InvoiceNumber" # the shared hub field this maps to
required = true
normalize = ["trim", "empty_as_missing"]
```

The full authoring reference — `[meta]`, node fields, types, normalization,
collections, fallbacks, constants, clones, inheritance, auto-detection, and
every diagnostic code — lives in
**[config/mappings/README.md](config/mappings/README.md)**. See
[config/mappings/ubl.toml](config/mappings/ubl.toml) and
[config/mappings/xrechnung.toml](config/mappings/xrechnung.toml) for the reference spokes.

---

## Adding a new format

1. Write a new `config/mappings/<your-format>.toml` with a `[meta]` table and your
   nodes — the DSL reference is [config/mappings/README.md](config/mappings/README.md), and
   the reference spokes make good templates. If your format is a profile
   of an existing syntax, `inherits` its mapping and declare only the deltas —
   see [config/mappings/peppol.toml](config/mappings/peppol.toml) for the minimal case.
2. Give it the same `canonical_key`s (with matching types) as the existing
   spokes for everything you want to round-trip; add new keys for fields unique
   to your format.
3. Declare the format's XSD in `[meta.schema]` (vendor it under
   [testfiles/xsd/](testfiles/xsd/)) and, ideally, a sample document in
   `[[meta.samples]]`: every format's output is then validated against it
   and round-tripped — see
   [Schema conformance](config/mappings/README.md#schema-conformance).
4. Rebuild:

   ```bash
   cargo build --release -p einvoice-interfaces
   ```

The build scans `config/mappings/`, compiles your file through the DSL pipeline,
derives the shared hub, and generates the mapper. Your format then appears in
`--list` and is usable as a source or target — no Rust changes required.

If your mapping has a problem (unknown key, type conflict, fallback cycle, …),
the **build fails** with a diagnostic pointing at the offending node.

---

## Performance

KrabInvoice converts a typical invoice in about 1 ms. One CPU core handles
roughly 4,400 typical invoices per second, throughput grows almost linearly
with CPU cores, and the stateless service scales out across instances.
There is no fixed invoice size limit (100,000-line, 89 MB invoices convert
in about 2 s), and memory stays bounded under any load: invoices queue
instead of failing.

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for throughput and scaling
charts, memory needs per invoice size, and a deployment sizing guide.

---

## Workspace layout

| Crate | Role |
|-------|------|
| [crates/einvoice-dsl](crates/einvoice-dsl/src/README.md) | Build-time mapping compiler: TOML → IR → validation → generated Rust hub + mappers |
| [crates/einvoice-transformator](crates/einvoice-transformator/src/README.md) | Pure runtime helpers (normalization, validation, diagnostics) the generated mappers link against |
| [crates/einvoice-interfaces](crates/einvoice-interfaces/src/README.md) | Public `Engine` API, the generated registry, and the `krab-cli` CLI |

The TOML never reaches the runtime: `einvoice-interfaces`'s `build.rs` compiles
[config/mappings/](config/mappings/) through `einvoice-dsl` into native Rust, and the engine
executes only that generated code.

---

## Developer commands

Emitted documents are validated against the vendored XSDs in
[testfiles/xsd/](testfiles/xsd/) by `crates/einvoice-interfaces/tests/xsd_validation.rs`
(it needs `xmllint` from libxml2 on `PATH`; CI installs it). The test names no
format: it runs the checks each mapping declares in `[meta.schema]` and
`[[meta.samples]]`, the same ones `krab-cli --check` reports. A format's
remaining schema errors are its `known_gaps`, and the check fails both on a
new error and on a stale gap, so the lists only shrink.

Install the local pre-commit hooks once per checkout:

```bash
pre-commit install
```

Run the same hooks manually across the workspace with:

```bash
pre-commit run --all-files
```

The DSL crate ships an `xtask` dev CLI for mapping authors. It loads the
mappings through the exact same loader and compiler the build uses, so what
`check` accepts, the build accepts:

```bash
# Compile every mapping and print all diagnostics
cargo run -p einvoice-dsl -- check config

# Print a canonical coverage matrix and gap report
cargo run -p einvoice-dsl -- report config
```

`check` exits non-zero when any error-severity diagnostic is produced. `report`
is a static authoring report over the TOML spokes; it does not require an input
invoice.
