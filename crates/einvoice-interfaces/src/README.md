# einvoice-interfaces

## Purpose

`einvoice-interfaces` is the **public engine API and CLI** — the crate that wires
the build-time compiler (`einvoice-dsl`) to the runtime helpers
(`einvoice-transformator`). Its `build.rs` scans the workspace `mappings/`
directory, resolves each spoke's inheritance chain (ancestor-first; a
`[meta].disabled = true` mapping stays resolvable as a parent but emits no
spoke), compiles everything through `einvoice_dsl::compile`, and generates the
typed hub, one mapper module per spoke, and the `Spoke` registry into `OUT_DIR`.
No format is named in hand-written code.

## Structure

- `build.rs` — mapping discovery, inheritance-chain resolution, compilation, and
  code generation (`hub.rs` + `spokes.rs` in `OUT_DIR`).
- `lib.rs` — [`Engine`] (`to_hub`, `from_hub`, `transform`), [`EngineError`], and
  the re-exported generated [`Spoke`] enum and [`MainKey`] hub.
- `analysis.rs` — static conversion analysis (the CLI's `--analyze`): the
  loss/error state of every source x target pair, without an input document.
- `keys.rs` — canonical-key reporting (the CLI's `--keys`): the hub vocabulary,
  and per-spoke covered/unused keys.
- `table.rs` — shared aligned-table rendering used by `analysis` and `keys`.
- `cli/` — the `krab-cli` CLI: argument parsing, format resolution, source
  auto-detection, IO wiring, and diagnostic rendering (see its `mod.rs` docs).
- `server/` — the `krab-server` HTTP surface: env/hardware configuration, the
  global memory-budget admission gate, and the request → response mapping
  (see its `README.md`).
- `bin/krab-cli.rs` — thin binary shell forwarding argv and the standard
  streams into `cli::run`.
- `bin/krab-server.rs` — thin binary shell binding `tiny_http` workers to the
  `server` module.

## Behavior

`Engine::transform` is the N–1–N path: deserialize source bytes into the
generated typed source struct, run the generated reader to the typed `MainKey`
hub, run the target's generated writer, serialize back to XML. Mapping-level
outcomes (missing required fields, type errors, taken fallbacks) are
`MappingDiagnostic`s in the returned `MappingResult`; an `EngineError` means the
bytes could not be parsed or rendered at all.

## Testing

`lib.rs` carries end-to-end tests over the generated mappers (read, round-trip,
diagnostics, malformed input). `analysis`, `keys`, `table`, and the `cli`
submodules carry in-module unit tests; CLI behavior is tested through
`cli::run` against the generated registry.
