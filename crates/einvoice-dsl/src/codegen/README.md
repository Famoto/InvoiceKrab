# codegen

## Purpose

The `codegen` module is the bridge from the compiler's static artifacts to
runtime code: it emits **Rust source text** for the canonical hub and for each
spoke's source record functions and `read`/`write` mappers. The runtime never
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
- `source.rs` — one unit struct per source element with generated
  `read_xml` / `write_xml` / `is_empty` over the packed record (plus
  `demux` / `mux` for aliased elements), and `from_xml` / `to_xml`.
- `read.rs` — the reader: source → `MainKey`.
- `write.rs` — the writer: `MainKey` → source (inverse of the reader).
- `access.rs` — the record layout (`size`, `fields`: which id each source
  path gets) and the access expressions over it shared by reader and writer
  (`resolve`, `access_expr`, `slot`, `normalize_chain`, `collection_item_struct`).
- `plan.rs` — IR classification: root scalars/collections/constants/clones and
  the children / nested collections / constants / clones of any collection
  node, in deterministic id order.
- `diag.rs` — emission of `MappingDiagnostic` construction snippets.

## Behavior

The generators are **pure and deterministic**: identical inputs yield
byte-identical output (all `BTreeMap`s are iterated in sorted order). Invariant:
refactors must not change generated text.

The fields *inside* a generated source element are the one place sorted order
is not used: they follow `StructMeta::ordered_fields` — attributes, then
`$text`, then child elements by declaration position. The generated `write_xml`
writes them in that order, so the emitted XML follows the mapping's (and the
XSD's) element sequence.

### The source record

A source document is not a tree of structs but one packed byte buffer
(`einvoice_transformator::record::Values`): a flat run of
`varint(id) varint(len) payload` entries holding only the values the document
has. The mapping already fixes the structure, so the structure is not stored:
it becomes the **layout**, a number per field, and the generated code is the
schema. An element at `base` takes the ids `base..base + size`: `base` itself
marks it present, then one id per field in emission order. A scalar (single
or repeated) or a repeated element takes one id; a single child element is
inlined (its own ids, no nesting); the items of a repeated element are nested
records under one id, each laid out from 0. An absent field costs nothing; a
present one costs its bytes plus two to four bytes of header. `access.rs`
computes the layout (`size`, `fields`), and every source path in the reader
and writer is resolved to a literal id at generation time (`source.get(7)`,
`source.items(3)`, `element0.set(2, &rendered)`), so nothing is looked up by
name at runtime.

`read_xml` streams the document through quick-xml's `NsReader`
(`einvoice_transformator::xml::Reader`) straight into the record: an element
it maps becomes an entry, everything else is skipped without being kept. It
matches what quick-xml's serde deserializer accepted and rejected (local-name
matching, `duplicate field` on a second single element or a broken repeated
run, `xsi:nil`, mixed content, unclosed tags), so diagnostics and errors are
unchanged; the old build is the oracle a differential corpus run compares
against.

`write_xml` writes each element straight into one output `String`, its tag
names as literals, with no per-element child buffer. An element is written
when any id in its range holds an entry, so an empty element stays absent
unless it was marked (read from an empty element in the document, or a
`required` structural node).

Namespaces are a write-side concern. The writer emits each element's prefixed
name (`cbc:ID`) while the reader matches local names. The root's `write_xml`
writes one `xmlns` declaration per declared namespace before its fields, and
`to_xml` writes the XML declaration and the root tag qualified with
`root_ns`.

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
stays a borrow instead of being taken), and the reader — after every
primary assign in the scope — decodes the copy only to compare it against the
canonical value, warning `CLONE_MISMATCH` when a document's copies disagree.
The hub is never filled from a clone. A `$parent.Key` / `$root.Key` clone
reads (and compares against) the enclosing scope's / the root's hub value;
the planner records such keys so their primaries borrow instead of move.

A physical element bound by several logical nodes (`match` selectors, `xml`
aliases) is one repeated field on the wire plus one *logical* field per node.
The reader keeps the record as read and picks each logical field's items out
of the physical ones (`Owner::class_<field>(item, refs)` names the first
logical field whose selector an item satisfies); `demux` only reports
`MATCH_MULTIPLE` when a single-valued node matched more. The writer lays the
logical fields out in ids of their own and calls `mux` last, which rebuilds
the record with the logical items moved into the physical id, in declaration
order, and the selector values written as discriminators.

## Testing

Unit tests live in `mod.rs` and assert on the generated hub + spoke for the
reference UBL mapping, including the nested-collection case. The layout and
`snake_case` have focused unit tests. A determinism test pins repeatability.
