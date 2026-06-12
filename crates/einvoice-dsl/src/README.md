# einvoice-dsl

## Purpose

`einvoice-dsl` is the **build-time mapping compiler** for KrabInvoice's TOML
mapping DSL. It parses a spoke's TOML mapping file, resolves it into a
normalized IR, **synthesizes the typed source model from the mapping nodes**,
derives the canonical hub from the spokes, statically validates everything,
exposes reporting data for authoring tools, and **generates native Rust hub and
mapper code**.

There is a single tree: each node's dotted id mirrors the XML element tree (its
segments are XML element local names), so the compiler derives the Rust source
struct tree and each node's `source_path` from the ids — there is no separate
`[source]` table and no hand-written `path`. The `xml` field on a node only marks
a leaf as an attribute (`@currencyID`) or element text, or renames an element.

The guiding principle is **fail at build time, not at runtime**: unknown TOML
keys, fallback cycles, and cross-spoke type conflicts are all compile-time
errors. The runtime executes only generated Rust, never interpreted TOML. This
crate has **no dependency on the runtime crate** (`einvoice-transformator`);
codegen emits text that *targets* the runtime's API by name.

## Structure (pipeline order)

| File | Role |
|---|---|
| `types.rs` | `MappingType` — the closed set of source-node value types. |
| `normalize.rs` | `NormalizeOp` — declared string transforms. |
| `multiple.rs` | `MultiplePolicy` — repeated-scalar handling. |
| `meta.rs` | `MappingMeta` — the `[meta]` table (identity, `root`, `inherits`, `detect`, inherit-only `disabled`). |
| `node.rs` | `NodeId` / `Scope` / `RawNode` (as-declared) / `SourceNode` (effective). |
| `error.rs` | `ConfigError`, `Diagnostic`, `Severity`. |
| `parse.rs` | TOML → `ParsedMapping` (dotted-table flattening). |
| `resolve.rs` | inheritance merge → disabled removal → default materialization. |
| `ir.rs` | `MappingIr` + `build_ir` (the normalized mapping + synthesized source model). |
| `source_model/` | `SourceModelMeta` (metadata types), path resolution (`resolve_path`), and `synthesize_source_model` (struct tree + source paths from the nodes) — split into `meta.rs` / `resolve.rs` / `synth.rs`. |
| `hub.rs` | `derive_hub` — the canonical model as the union of spoke `canonical_key`s. |
| `validate.rs` | the compile-time validation pipeline (E020–E050). |
| `compile.rs` | `compile` — runs the whole multi-spoke pipeline and aggregates diagnostics. |
| `report.rs` | Static reporting helpers: coverage matrix, gap report, fallback graph. |
| `codegen/` | `generate_hub` and `generate_spoke` — emit the typed hub plus native Rust reader/writer modules. |

## How the pieces fit together

```
TOML mappings ─► parse ─► resolve(inherit, disabled)
              ─► synthesize source models ─► IRs
                                      │
                                      ├─► derive_hub ─► validate
                                      │                  │
                                      └──────────────────┴─► report / codegen
```

1. `parse_mapping` turns one TOML document into a `ParsedMapping`, rejecting
   unknown keys (E001).
2. `build_ir` resolves the inheritance chain, drops disabled nodes, **synthesizes
   the `SourceModelMeta` and each node's `source_path` from the node ids**, and
   materializes defaults into a deterministic `MappingIr`.
3. `derive_hub` folds every spoke's `canonical_key`s into a `CanonicalModel`,
   enforcing cross-spoke type/scope consistency.
4. `validate` checks canonical scopes, fallbacks (existence, type, cycles), and
   adapters; the synthesized source model is consistent by construction.
5. `compile` aggregates diagnostics from every stage in deterministic order.
6. `report` renders comparison views; `generate_hub` emits the typed `MainKey`
   hub, and `generate_spoke` emits the reader (source→hub) and writer
   (hub→source) as Rust source text.

## Testing

Every module carries in-module `#[cfg(test)] mod tests` (strict TDD per the root
`CLAUDE.md`). Parametrized cases use `rstest`; every E-code has an error-path
test. `source_model/synth.rs` covers synthesis (valued containers, attributes,
collections, source-path values, determinism) directly.
