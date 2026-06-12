# codegen

## Purpose

The `codegen` module is the bridge from the compiler's static artifacts to
runtime code: it emits **Rust source text** for the canonical hub and for each
spoke's typed source structs and `read`/`write` mappers. The runtime never
interprets the TOML — it links against this generated Rust.

What does not belong here: parsing, IR building, hub derivation, or any runtime
behavior. This module only turns already-validated artifacts into deterministic
text.

## Structure

- `mod.rs` — module docs, the public entry points `generate_hub`
  (re-exported from `hub`) and `generate_spoke`, and the tests.
- `naming.rs` — canonical-key → Rust identifier conversions (`snake_case`,
  `field_name`, `item_struct_name`) and the canonical-type → Rust-type map.
  These are intentionally distinct from the XML-name / `doc_format` conversions
  elsewhere in the crate (canonical keys are PascalCase, no acronym handling).
- `hub.rs` — `generate_hub`: the `MainKey` struct + one item struct per
  canonical collection.
- `source.rs` — typed source structs (with serde/XML binding) and `from_xml` /
  `to_xml`.
- `read.rs` — the reader: source → `MainKey`.
- `write.rs` — the writer: `MainKey` → source (inverse of the reader).
- `access.rs` — source-path access expressions shared by reader and writer
  (`walk_segments`, `access_expr`, `normalize_chain`, `collection_item_struct`).
- `plan.rs` — IR classification: root scalars/collections and the children /
  nested collections of any collection node, in deterministic id order.
- `diag.rs` — emission of `MappingDiagnostic` construction snippets.

## Behavior

The generators are **pure and deterministic**: identical inputs yield
byte-identical output (all `BTreeMap`s are iterated in sorted order). Invariant:
refactors must not change generated text.

A source path that fails to resolve (which validation E021/E023 should have
rejected) emits a `compile_error!` at the access site rather than plausible but
wrong code, so a validation/codegen gap fails loudly with a clear message.

## Testing

Unit tests live in `mod.rs` and assert on the generated hub + spoke for the
reference UBL mapping, including the nested-collection case. `serde_attr` and
`snake_case` have focused unit tests. A determinism test pins repeatability.
