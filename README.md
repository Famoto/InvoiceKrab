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

Adding a new format is just dropping a new `*.toml` into [mappings/](mappings/).
No Rust code names any format by hand. A CIUS (a national/sector profile of a
base syntax) can *inherit* another mapping's whole tree and restate only its
deltas — XRechnung and Peppol are a handful of lines on top of UBL.

---

## Table of contents

- [Installation](#installation)
- [Quick start](#quick-start)
- [Bundled mappings](#bundled-mappings)
- [The `krab-cli` CLI](#the-krab-cli-cli)
- [The `krab-server` HTTP API](#the-krab-server-http-api)
- [Library usage](#library-usage)
- [Features](#features)
- [The mapping DSL](#the-mapping-dsl)
- [Adding a new format](#adding-a-new-format)
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

## Bundled mappings

The exact list is generated from [mappings/](mappings/) at build time and can be
checked with `krab-cli --list`. This workspace currently ships:

| Display name | Mapping file | Inherits | Notes |
|--------------|--------------|----------|-------|
| `ubl-invoice:2.1` | [mappings/ubl.toml](mappings/ubl.toml) | — | Base UBL Invoice tree, full EN 16931 model |
| `xrechnung-invoice:3.0.2` | [mappings/xrechnung.toml](mappings/xrechnung.toml) | `ubl-invoice:2.1` | XRechnung CIUS, detected by `CustomizationID` marker |
| `peppol-bis-billing:3.0` | [mappings/peppol.toml](mappings/peppol.toml) | `ubl-invoice:2.1` | Peppol BIS Billing CIUS, detected by `CustomizationID` marker |
| `facturx-invoice:1.0` | [mappings/facturx.toml](mappings/facturx.toml) | `cii-invoice:en16931` | Factur-X / ZUGFeRD, detected by guideline-id marker |
| `fatturapa:1.2.2` | [mappings/fatturapa.toml](mappings/fatturapa.toml) | — | Italian FatturaPA (`FatturaElettronica` tree) |

[mappings/cii.toml](mappings/cii.toml) carries the full UN/CEFACT CII tree but is
an **inherit-only base** (`[meta].disabled = true`): it exists to be inherited by
Factur-X/ZUGFeRD and emits no spoke of its own, so it does not appear in
`--list`.

---

## The `krab-cli` CLI

```
USAGE:
    krab-cli <INPUT> <TARGET-FORMAT> [--from <SOURCE-FORMAT>] [--out <FILE>]
    krab-cli --analyze [SOURCE-FORMAT]
    krab-cli --keys [FORMAT]
    krab-cli --list
    krab-cli --help

ARGS:
    <INPUT>            Source XML file, or `-` to read stdin
    <TARGET-FORMAT>    Format to emit (see --list)

OPTIONS:
    --from <FORMAT>    Source format; auto-detected when omitted
    --out <FILE>       Write to FILE instead of stdout
    --analyze          Report each transform's loss/error state
    --keys [FORMAT]    Show canonical main keys; with FORMAT, show that
                       spoke's covered and unused keys
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

The transformed XML is written to stdout (or `--out`). Mapping **diagnostics**
(warnings, info, errors) are written to **stderr**, so they never corrupt the
output stream. If the mapping produces any *error*-severity diagnostic, no
partial output is emitted and the process exits non-zero.

### List formats

```bash
krab-cli --list
```

Prints every format compiled into this build (one per `mappings/*.toml`).

### Analyze conversions (no input needed)

`--analyze` statically reports the loss/error state of every conversion — which
target formats can represent everything a source carries, and which would drop
fields — *without* needing an actual document.

```bash
# Full source x target matrix
krab-cli --analyze

# Scope to "from UBL to everything else"
krab-cli --analyze ubl-invoice
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

### Exit codes

KrabInvoice follows BSD `sysexits.h` conventions:

| Code | Meaning |
|------|---------|
| `0`  | Success (warnings/info may still appear on stderr) |
| `64` | Usage error — bad arguments, unknown format, or ambiguous source |
| `65` | Data error — input couldn't be parsed/rendered, or mapping had errors |
| `74` | I/O error — couldn't read input or write output |

---

## The `krab-server` HTTP API

The same transformation as an HTTP service, one document per request,
processed concurrently across a worker pool:

```bash
cargo run --release -p einvoice-interfaces --bin krab-server
# krab-server listening on 0.0.0.0:8080 — 16 workers, ... bytes memory budget, x5 reservation

curl -sS --data-binary @invoice.xml \
    'localhost:8080/transform?to=xrechnung-invoice&from=ubl-invoice'
```

`POST /transform?to=<format>[&from=<format>]` — body is the source XML;
`from` is auto-detected when omitted. `200` returns the transformed XML
(warning diagnostics in the `X-Krab-Warnings` header), `400` bad
parameters/XML, `422` mapping errors (rendered diagnostics in the body),
`411` missing Content-Length, `413` a request that could never fit the
memory budget.

Capability and health endpoints: `GET /formats` (JSON array of accepted
format names), `GET /analyze[?from=<format>]` (the CLI's `--analyze` table),
`GET /health` (`200 ok`; `krab-server --healthcheck` self-probes it for the
Docker `HEALTHCHECK`).

Configuration is environment variables; defaults derive from the actual
hardware (cgroup-aware, so container limits are respected):

| Variable                | Default                                       |
|-------------------------|-----------------------------------------------|
| `KRAB_ADDR`             | `0.0.0.0:8080`                                |
| `KRAB_WORKERS`          | available parallelism                         |
| `KRAB_MEM_BUDGET_BYTES` | detected memory x 1/2 (cgroup v2 limit first) |
| `KRAB_MEM_BLOWUP`       | `5` — reservation = Content-Length x blowup   |

There is no per-document size limit. Instead, each request reserves
`Content-Length x KRAB_MEM_BLOWUP` bytes from a global budget before its
body is read; requests run in parallel while budget remains and queue when
it is exhausted, so request traffic can never drive the process out of
memory. See [crates/einvoice-interfaces/src/server/README.md](crates/einvoice-interfaces/src/server/README.md).

The Dockerfile ships both programs: `docker build --target server` for the
HTTP service (default), `--target cli` for the CLI image. All knobs are
runtime environment variables — set them per container, never at build time:

```bash
docker build --target server -t krab-server .

# Defaults derive from the container's own limits: workers from --cpus,
# memory budget = half of --memory (cgroup v2).
docker run --rm -p 8080:8080 --cpus 4 --memory 2g krab-server

# Explicit overrides win over detection.
docker run --rm -p 8080:8080 \
    -e KRAB_WORKERS=8 \
    -e KRAB_MEM_BUDGET_BYTES=1000000000 \
    -e KRAB_MEM_BLOWUP=4 \
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
- **Declarative TOML mappings.** A format is described by data, not code. The
  node ids mirror the XML element tree, so you describe *what* maps where, never
  *how* to walk the document.
- **Mapping inheritance.** A CIUS spoke inherits its base syntax's whole tree
  and restates only its deltas. A base can be inherit-only
  (`[meta].disabled = true`) so it never emits a spoke itself.
- **Compile-time safety.** The mapping compiler fails the build on unknown keys,
  fallback cycles, missing types, and cross-format type conflicts. If it builds,
  the mappers are type-checked Rust.
- **No runtime interpretation.** TOML is compiled to native Rust mappers at
  build time; at run time the engine only executes generated code.
- **Generated format registry.** The build scans `mappings/*.toml` and derives
  the public `Spoke` enum, module names, display names, and detection markers.
- **Source auto-detection.** Omit `--from` and KrabInvoice identifies the source
  format, disambiguating specifications/CIUS by the document's `CustomizationID`.
- **Diagnostics, not silent loss.** Missing required fields, type errors, and
  taken fallbacks are reported as structured diagnostics with severity and a
  source-node reference — they don't vanish.
- **Static conversion analysis.** `--analyze` shows what each conversion would
  lose before you run it.
- **Canonical key authoring aid.** `--keys` shows the hub vocabulary and, for one
  format, which existing keys are still unmapped.
- **Namespace-agnostic XML.** Mappings bind XML *local* names, so the same
  mapping reads real namespaced (`cbc:`/`cac:`) UBL and bare-name fixtures.
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
**[mappings/README.md](mappings/README.md)**. See
[mappings/ubl.toml](mappings/ubl.toml) and
[mappings/xrechnung.toml](mappings/xrechnung.toml) for the reference spokes.

---

## Adding a new format

1. Write a new `mappings/<your-format>.toml` with a `[meta]` table and your
   nodes — the DSL reference is [mappings/README.md](mappings/README.md), and
   the reference spokes make good templates. If your format is a profile
   of an existing syntax, `inherits` its mapping and declare only the deltas —
   see [mappings/peppol.toml](mappings/peppol.toml) for the minimal case.
2. Give it the same `canonical_key`s (with matching types) as the existing
   spokes for everything you want to round-trip; add new keys for fields unique
   to your format.
3. Rebuild:

   ```bash
   cargo build --release -p einvoice-interfaces
   ```

The build scans `mappings/`, compiles your file through the DSL pipeline,
derives the shared hub, and generates the mapper. Your format then appears in
`--list` and is usable as a source or target — no Rust changes required.

If your mapping has a problem (unknown key, type conflict, fallback cycle, …),
the **build fails** with a diagnostic pointing at the offending node.

---

## Workspace layout

| Crate | Role |
|-------|------|
| [crates/einvoice-dsl](crates/einvoice-dsl/src/README.md) | Build-time mapping compiler: TOML → IR → validation → generated Rust hub + mappers |
| [crates/einvoice-transformator](crates/einvoice-transformator/src/README.md) | Pure runtime helpers (normalization, validation, diagnostics) the generated mappers link against |
| [crates/einvoice-interfaces](crates/einvoice-interfaces/src/README.md) | Public `Engine` API, the generated registry, and the `krab-cli` CLI |

The TOML never reaches the runtime: `einvoice-interfaces`'s `build.rs` compiles
[mappings/](mappings/) through `einvoice-dsl` into native Rust, and the engine
executes only that generated code.

---

## Developer commands

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
cargo run -p einvoice-dsl -- check mappings

# Print a canonical coverage matrix and gap report
cargo run -p einvoice-dsl -- report mappings
```

`check` exits non-zero when any error-severity diagnostic is produced. `report`
is a static authoring report over the TOML spokes; it does not require an input
invoice.
