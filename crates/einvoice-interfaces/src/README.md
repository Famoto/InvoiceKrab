# einvoice-interfaces

## Purpose

`einvoice-interfaces` is the **public engine API and CLI** — the crate that wires
the build-time compiler (`einvoice-dsl`) to the runtime helpers
(`einvoice-transformator`). Its `build.rs` loads the workspace `config/`
directory — the shared codecs in `config/codecs/`, then the `config/mappings/`
directory, resolves each spoke's inheritance chain (ancestor-first; a
`[meta].disabled = true` mapping stays resolvable as a parent but emits no
spoke), compiles everything through `einvoice_dsl::compile`, and generates the
typed hub, one mapper module per spoke, and the `Spoke` registry into `OUT_DIR`.
No format is named in hand-written code.

## Structure

- `build.rs` — mapping discovery, inheritance-chain resolution, compilation, and
  code generation (`hub.rs` + `spokes.rs` in `OUT_DIR`, the registry carrying
  each spoke's embedded contract and its declared schema and samples).
- `lib.rs` — [`Engine`] (`to_hub`, `from_hub`, `transform`), [`EngineError`], and
  the re-exported generated [`Spoke`] enum and [`MainKey`] hub.
- `encoding.rs` — source character encodings: UTF-16, ISO-8859-1,
  windows-1252 and US-ASCII documents are transcoded to UTF-8 (UTF-8 passes
  through uncopied) before `Engine::to_hub` and auto-detection see them.
- `identity.rs` — source document identity: the `Identity` `build.rs` embeds
  per spoke (`Spoke::identity()`: root namespace URI and local name, exact
  profile identifiers, versions, root attributes) and its `check`, run by
  `Engine::to_hub` on every read and matched by auto-detection.
- `contract.rs` — the runtime `TransformationContract` types: what a spoke
  maps, what its `required` nodes need to write, what it declares it may lose.
  `build.rs` embeds one per spoke (`Spoke::contract()`).
- `analysis.rs` — static conversion analysis (the CLI's `--analyze`): compares
  two contracts into the loss/error state of a pair plus its findings
  (missing required routes, type clashes, dropped keys, optional feeds, pins,
  recodes, collapses), without an input document.
- `keys.rs` — canonical-key reporting (the CLI's `--keys`): the hub vocabulary,
  and per-spoke covered/unused keys.
- `conformance.rs` — schema conformance (the CLI's `--check`): derives from
  each spoke's `Spoke::schema` and `Spoke::samples` the sample-validity,
  emitted-validity (up to `known_gaps`) and round-trip checks, validating with
  `xmllint`.
- `table.rs` — shared aligned-table rendering used by `analysis` and `keys`.
- `cli/` — the `krab-cli` CLI: argument parsing, format resolution, source
  auto-detection, IO wiring, and diagnostic rendering (see its `mod.rs` docs).
- `server/` — the `krab-server` HTTP surface: env/hardware configuration, the
  global memory-budget admission gate, and the request → response mapping
  (see its `README.md`).
- `bin/krab-cli.rs` — thin binary shell forwarding argv and the standard
  streams into `cli::run`.
- `bin/krab-server.rs` — thin binary shell binding a tokio runtime, listener,
  and shutdown signals to the `server` module's axum router.

## Behavior

`Engine::transform` is the N–1–N path: deserialize source bytes into the
generated typed source struct, run the generated reader to the typed `MainKey`
hub, run the target's generated writer, serialize back to XML. Mapping-level
outcomes (missing required fields, type errors, taken fallbacks) are
`MappingDiagnostic`s in the returned `MappingResult`; an `EngineError` means the
bytes could not be parsed or rendered at all.

## Testing

`lib.rs` carries end-to-end tests over the generated mappers (read, round-trip,
diagnostics, malformed input). `analysis`, `conformance`, `keys`, `table`, and
the `cli` submodules carry in-module unit tests; CLI behavior is tested through
`cli::run` against the generated registry; `contract` checks every embedded
contract for internal consistency. `tests/xsd_validation.rs` runs the
conformance checks the mappings declare over the whole registry.
