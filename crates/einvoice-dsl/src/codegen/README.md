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
  `item_struct_name`) and the canonical-type → Rust-type map.
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
- `plan.rs` — IR classification: root scalars/collections/constants/clones and
  the children / nested collections / constants / clones of any collection
  node, in deterministic id order.
- `diag.rs` — emission of `MappingDiagnostic` construction snippets.

## Behavior

The generators are **pure and deterministic**: identical inputs yield
byte-identical output (all `BTreeMap`s are iterated in sorted order). Invariant:
refactors must not change generated text.

The fields *inside* a generated source struct are the one place sorted order is
not used: they follow `StructMeta::ordered_fields` — attributes, then `$text`,
then child elements by declaration position. serde serializes struct fields in
declaration order, so this is what makes the emitted XML follow the mapping's
(and the XSD's) element sequence.

Namespaces are a write-side concern. Every element field gets a split rename —
`rename(serialize = "cbc:ID", deserialize = "ID")` — so the writer emits the
prefixed name while the reader keeps matching local names. Each declared
namespace becomes a zero-size marker type (`XmlnsCbc`, serializing as its URI)
that the root struct carries as an `@xmlns:cbc` attribute field; `to_xml` writes
the XML declaration and the root tag qualified with `root_ns`.

A source path that fails to resolve (which validation E021/E023 should have
rejected) emits a `compile_error!` at the access site rather than plausible but
wrong code, so a validation/codegen gap fails loudly with a clear message.

A node with a `constant` is write-only from the hub's perspective: the writer
assigns the literal at the source path, the hub value — if the node also has a
`canonical_key` — is ignored on write, and the reader is unaffected. The
assignment is guarded on the constant's *owner* (the deepest interior element
it shares with real content) being non-empty; a constant with no owner is
unconditional at root and per non-empty item in a collection. Constants are
written last in their scope, after the content their guards inspect, and a
structural node with `required = true` is then materialized unconditionally.

A node with a `codec` decodes the source text through the codec's pattern on
read (`codec::decode_date(raw, "YYYYMMDD")`, `CODEC_INVALID` on mismatch,
`CODEC_WIRE_MISMATCH` when the document's wire attribute disagrees) and
encodes the canonical value on write, setting the codec's wire attributes
(`format = "102"`) on the element next to it.

A node with a `clone_of` mirrors an existing canonical key in its scope: the
writer fans the key's hub value out to the clone's path too (the key then
stays a borrow + clone instead of moving), and the reader — after every
primary assign in the scope — decodes the copy only to compare it against the
canonical value, warning `CLONE_MISMATCH` when a document's copies disagree.
The hub is never filled from a clone. A `$parent.Key` / `$root.Key` clone
reads (and compares against) the enclosing scope's / the root's hub value;
the planner records such keys so their primaries borrow instead of move.

A physical element bound by several logical nodes (`match` selectors, `xml`
aliases) is one repeated field on the wire plus one `#[serde(skip)]` *logical*
field per node. `source.rs` emits `demux`/`mux` on every struct holding such an
element: the reader calls `source.demux(..)` first, which hands each item to
the first logical field whose selector it satisfies (warning `MATCH_MULTIPLE`
when a single-valued node matched more), and the writer calls `source.mux()`
last, which moves the logical items back and writes the selector values as
discriminators. The read/write generators themselves see only ordinary paths
through the logical fields.

## Testing

Unit tests live in `mod.rs` and assert on the generated hub + spoke for the
reference UBL mapping, including the nested-collection case. `serde_attr` and
`snake_case` have focused unit tests. A determinism test pins repeatability.
