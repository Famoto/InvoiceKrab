# The KrabInvoice Mapping DSL

Every document format KrabInvoice speaks is described by one TOML file in this
directory (`config/mappings/`) — a **spoke**. The lexical codecs all spokes
share live beside it in [`config/codecs/`](../codecs/). At build time the DSL compiler turns each spoke into
native, type-checked Rust: a typed source struct, a `read` mapper (XML → canonical
hub) and a `write` mapper (hub → XML). The runtime never sees the TOML.

This is the complete authoring reference. The guiding principle throughout is
**fail at build time, not at runtime**: unknown fields, type conflicts, broken
fallbacks, and invalid constants are all compile errors with a diagnostic
pointing at the offending node.

## Table of contents

- [How a mapping becomes code](#how-a-mapping-becomes-code)
- [The big idea: ids mirror the XML tree](#the-big-idea-ids-mirror-the-xml-tree)
- [Declaration order is schema order](#declaration-order-is-schema-order)
- [Namespaces](#namespaces)
- [The `[meta]` table](#the-meta-table)
- [Source nodes](#source-nodes)
  - [Node fields reference](#node-fields-reference)
  - [The `xml` field: attributes, text, renames](#the-xml-field-attributes-text-renames)
- [Types](#types)
- [Normalization](#normalization)
- [Multiple values](#multiple-values)
- [Collections & scopes](#collections--scopes)
- [Structural matching: one element, several nodes](#structural-matching-one-element-several-nodes)
- [Canonical keys & the hub](#canonical-keys--the-hub)
- [Fallbacks](#fallbacks)
- [Constants: pinning write-side values](#constants-pinning-write-side-values)
- [Clones: one value, several places](#clones-one-value-several-places)
- [Codecs](#codecs)
- [Read defaults](#read-defaults)
- [Derivations: EN 16931 calculation rules](#derivations-en-16931-calculation-rules)
- [`required` and the transformation contract](#required-and-the-transformation-contract)
- [Inheritance](#inheritance)
- [Auto-detection](#auto-detection)
- [Schema conformance](#schema-conformance)
- [A complete example](#a-complete-example)
- [Checking your mapping](#checking-your-mapping)
- [Diagnostic code reference](#diagnostic-code-reference)

---

## How a mapping becomes code

The workspace build (`einvoice-interfaces`'s `build.rs`) scans this directory
and compiles every `*.toml` through the DSL pipeline:

1. **Parse** — strict TOML deserialization; unknown fields are rejected.
2. **Inheritance** — the `inherits` chain is resolved ancestor-first and folded
   into one effective node set per spoke.
3. **Source-model synthesis** — the typed Rust source struct tree is derived
   from the node ids. You never write a struct.
4. **Hub derivation** — the canonical model (`MainKey`) is computed as the
   union of every spoke's `canonical_key`s, with cross-spoke consistency checks.
5. **Validation** — paths, fallbacks, scopes, constants, clones and codecs are
   checked, then every spoke's `required` write routes against the other
   spokes; every problem is reported (never just the first).
6. **Codegen** — the `read`/`write` mappers and the format registry (the
   `Spoke` enum, display names, detection markers, each spoke's
   [transformation contract](#required-and-the-transformation-contract) and
   its [schema-conformance](#schema-conformance) declarations) are emitted as
   Rust.

Two spokes round-trip through the hub precisely because they share canonical
keys — adding a spoke makes it interoperable with *all* existing formats, with
no per-pair conversion code.

---

## The big idea: ids mirror the XML tree

There is a **single tree**. Each node's dotted TOML table id *is* its XML
element path under the root. The compiler derives the source struct tree and
each node's XML path from the ids — there is no separate `[source]` table and
no hand-written path.

```toml
[Invoice.LegalMonetaryTotal.PayableAmount]   # <Invoice><LegalMonetaryTotal><PayableAmount>
```

Interior elements (`LegalMonetaryTotal` here) are *inferred* from the ids of
their leaf descendants — you never declare them as their own table. On the read
side, missing interior elements simply mean the leaves under them are missing.

Every id segment and every `xml` binding must be an XML name (an `NCName`:
a letter or `_`, then letters, digits, `-`, `.` or `_`; no prefix) — E026.
The generated Rust absorbs the rest: `-` and `.` become `_` in field names, a
Rust keyword gets a trailing `_` (`<type>` → `type_`, `<Ref>` → `ref_`), and an
element whose struct name would clash with another path's (`A.BC` vs `AB.C`)
or with a type the generated code uses (`Option`, `Decimal`, the root's own
name) gets an `Element` suffix. One limitation remains: a segment spelled like
a node field (`type`, `xml`, `match`, `ns`, …) is read as that field, so bind
such an element under another id with `xml = "match"`. `[meta].root` names the
root struct verbatim, so it must also be a plain Rust type name (E026).

A node id may omit the root segment (`[InvoiceLine]` is
`[Invoice.InvoiceLine]`), so `[ID]` and `[Invoice.ID]` are the *same* element:
mapping it twice is E025, as is any second node bound to one element or
attribute through `xml`.

XML matching is **namespace-agnostic** on the read side: mappings bind XML
*local* names, so the same mapping reads real namespaced UBL (`cbc:ID`,
`cac:LegalMonetaryTotal`) and bare-name test fixtures alike. On the write side
the document is fully qualified — see [Namespaces](#namespaces).

---

## Declaration order is schema order

XML schemas define their children as *sequences*, so a valid document must emit
sibling elements in the schema's order. The DSL has no separate ordering
syntax: **the order in which you declare the tables is the order the writer
emits the elements.** Keep each group of siblings in XSD sequence order and the
output validates.

The rules, all applied by the compiler:

- Siblings are emitted in the order of their declaring tables. The map of nodes
  is sorted by id internally, but every node remembers its declaration
  position, and the generated source structs (hence serde, hence the XML)
  follow that position.
- An inferred interior element (`LegalMonetaryTotal`, `Party`, …) sits where its
  **first** declared descendant is. Later leaves under it do not move it, so a
  parent's children stay contiguous wherever they are declared. A structural
  node counts as that first appearance, so declare it where the element belongs
  in the sequence, not at the top of the file.
- Attributes are emitted before child elements; an element's own text
  (`$text`) comes before its children. Neither has a schema order.
- Reading is order-independent: a source document may list elements in any
  order and still deserializes.
- Under [inheritance](#inheritance), an override keeps the base node's position
  (restating a node never moves the element). Nodes new to the child are
  appended after all of the base's nodes, in the child's declaration order.

Because CII puts `IncludedSupplyChainTradeLineItem` *before* the header trade
groups inside `SupplyChainTradeTransaction`, [cii.toml](cii.toml) declares the
invoice-line section before the header sections — the file order follows the
schema, not the reading order a human might prefer.

---

## Namespaces

A schema-valid document declares its namespaces on the root and qualifies every
element. Three optional `[meta]` entries describe that; all three are
**inherited** from the parent mapping when a child omits them, so a CIUS
declares nothing:

```toml
[meta]
root_ns = ""                 # prefix of the root element ("" = default namespace)

[meta.namespaces]            # declared on the root as xmlns / xmlns:prefix
""  = "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"
cbc = "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2"
cac = "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2"

[meta.ns_defaults]
leaf      = "cbc"            # scalar and valued elements
aggregate = "cac"            # inferred interior elements and collections
```

Per node, `ns = "udt"` sets the prefix of **that node's own element**, overriding
the default for its kind. Attributes and `$text` are never prefixed (E081).
The empty prefix needs no declaration; every other prefix used by `root_ns`, the
defaults, or a node must appear in `[meta.namespaces]` (E080).

An inferred interior element that needs a prefix the defaults would not give it
is named by a **structural node**: a table with `ns` (plus optionally
`description`, or `required = true` to force its emission) and no `type`:

```toml
# CII: the root's three children are rsm:, everything beneath them ram:.
[CrossIndustryInvoice.ExchangedDocument]
ns = "rsm"
```

A structural node is consumed by the compiler when it creates that interior
element; it never maps a value, so it is an error for it to name nothing (no
typed node beneath it), to bind an attribute or `$text`, or to name the root
(whose prefix is `root_ns`) — E083. A `type`-less table with any *mapping*
field (`canonical_key`, `normalize`, `constant`, …) is still E002; only `ns`,
`xml`, `match`, `required`, `description` and `disabled` may stand alone.

What the writer emits: `<?xml version="1.0" encoding="UTF-8"?>`, the root tag
qualified with `root_ns` and carrying one `xmlns[:prefix]` attribute per declared
namespace, and every element qualified. Reading never changes: prefixes in the
source document are ignored, and the `xmlns` attributes are accepted and dropped.

Per format in this repository: UBL (and so XRechnung and Peppol) uses the
default namespace for `Invoice`, `cbc:` leaves and `cac:` aggregates; CII (and
so Factur-X) is `rsm:` for the root and its three children, `ram:` beneath, with
`udt:`/`qdt:` on the data-type leaves (`DateTimeString`, `Indicator`); FatturaPA
is `p:FatturaElettronica` with unqualified children.

---

## The `[meta]` table

The reserved `[meta]` table identifies the format. Unknown keys are rejected
(E001).

```toml
[meta]
doc_format      = "ubl-invoice"             # required — logical format id; drives the
                                            #   generated module name & Spoke variant
format_version  = "2.1"                     # required
mapping_version = "1.0"                     # required — version of this mapping file
canonical_model = "canonical-invoice:1.0"   # required — the hub this targets
root            = "Invoice"                 # root XML element / struct (default: "Root")
source_model    = "ubl-invoice:2.1"         # optional display id (default: doc_format:format_version)
detect          = ["xrechnung"]             # optional auto-detection markers
inherits        = "ubl-invoice:2.0"         # optional parent mapping to inherit from
disabled        = true                      # optional — inherit-only base, emits no spoke
description     = "…"                       # optional, reports only

[meta.schema]                               # optional — the XSD the format is defined by
xsd = "testfiles/xsd/ubl-2.1/maindoc/UBL-Invoice-2.1.xsd"

[[meta.samples]]                            # optional, repeatable — a document proving the mapping
file = "testfiles/xrechnung-3.0.2-beispiel.xml"
```

| Field | Required | Purpose |
|-------|----------|---------|
| `doc_format` | ✅ | Logical id; becomes the `Spoke` name and module slug |
| `format_version` | ✅ | Format version string |
| `mapping_version` | ✅ | Version of this mapping file |
| `canonical_model` | ✅ | Canonical model id this mapping targets |
| `root` | — | Root element/struct name (default `Root`) |
| `source_model` | — | Display id (default `doc_format:format_version`) |
| `detect` | — | Substrings matched against `CustomizationID` for [auto-detection](#auto-detection) |
| `inherits` | — | Parent mapping id to inherit nodes from |
| `disabled` | — | When `true`, inherit-only base: other spokes may `inherits` it, but it emits no `Spoke` of its own |
| `description` | — | Human note, used in reports only |
| `root_ns` | — | Prefix of the root element (default `""`); inherited — see [Namespaces](#namespaces) |
| `[meta.namespaces]` | — | `prefix = "URI"` declarations emitted on the root (`""` = default namespace); inherited |
| `[meta.ns_defaults]` | — | `leaf` / `aggregate` default prefixes; inherited |
| `[meta.schema]` | — | `xsd`, `catalog`, `known_gaps`: the schema the spoke's documents must satisfy; inherited — see [Schema conformance](#schema-conformance) |
| `[[meta.samples]]` | — | `file`, `source`: sample documents every spoke with a schema must write validly and round-trip; not inherited |

`source_model` is the id other mappings name in `inherits`. (The synthesized
model takes its id from it, so the E020 consistency check cannot fire from a
mapping file; it guards callers that supply source metadata separately.)
Duplicate mapping ids or slugs across
files, and unknown or cyclic `inherits` targets, fail the load before
compilation starts.

---

## Source nodes

Every table other than `[meta]` is a **source node**, keyed by its dotted id:

```toml
[Invoice.ID]
type = "identifier"
canonical_key = "InvoiceNumber"
required = true
normalize = ["trim", "empty_as_missing"]
```

A node plays one of five roles, depending on which fields it declares:

| Role | Declares | Read side | Write side |
|------|----------|-----------|------------|
| **Primary** | `canonical_key` | fills the hub key | emits the hub key's value |
| **Helper** | neither `canonical_key` nor `clone_of` | read only when referenced as a fallback | never written |
| **Constant** | `constant` (with or without `canonical_key`) | unchanged (fills hub if keyed) | always emits the fixed literal |
| **Clone** | `clone_of` | consistency check only | mirrors the target key's value |
| **Structural** | `ns`, `xml`, `match` and/or `required = true`, no `type` | nothing (or picks the element occurrence its `match` selects) | names an interior element's prefix (see [Namespaces](#namespaces)), forces its emission (see [Constants](#constants-pinning-write-side-values)), or binds one of several same-named elements (see [Structural matching](#structural-matching-one-element-several-nodes)) |

### Node fields reference

| Field | Type | Meaning |
|-------|------|---------|
| `type` | string | Value type (see [Types](#types)). Required for active nodes (E002). |
| `canonical_key` | string | Target field in the canonical hub. Omit for a helper node. |
| `xml` | string | Leaf binding override (see [below](#the-xml-field-attributes-text-renames)); on a collection or structural node, the physical element it binds (see [Structural matching](#structural-matching-one-element-several-nodes)). |
| `required` | bool | The node must have a deterministic write route and its value must be present (default `false`; see [`required`](#required-and-the-transformation-contract)). |
| `normalize` | array | String transforms, applied in order (see [Normalization](#normalization)). |
| `fallbacks` | array | Other node ids to try, in order, when this node is missing (see [Fallbacks](#fallbacks)). |
| `multiple` | string | Policy for repeated scalar values (see [Multiple values](#multiple-values)). |
| `join_with` | string | Separator — required iff `multiple = "join"` (E040). |
| `constant` | string | Fixed write-side literal (see [Constants](#constants-pinning-write-side-values)). |
| `clone_of` | string | Canonical key this node mirrors (see [Clones](#clones-one-value-several-places)). |
| `codec` | string | Id of a shared codec translating the hub value to the format's form: dates, booleans, code tables, number formats, character sets (see [Codecs](#codecs)). |
| `default` | string | Value read into the node's canonical key when the document lacks the element (see [Read defaults](#read-defaults)). |
| `description` | string | Human note, reports only. |
| `disabled` | bool | Remove this node from the effective mapping (useful with [inheritance](#inheritance)). |
| `ns` | string | Namespace prefix of this node's own element on write (see [Namespaces](#namespaces)). Alone on a `type`-less table it makes a structural node. |
| `match` | inline table | Selector on a collection or structural node: child value → literal, e.g. `{ "TypeCode" = "130" }` (see [Structural matching](#structural-matching-one-element-several-nodes)). Not valid on a scalar (E091). |
| `replace` | bool | Under [inheritance](#inheritance), replace the inherited node whole instead of merging over it. |

Any other field is rejected (E001) — typos never silently no-op.

### The `xml` field: attributes, text, renames

By default a leaf's XML local name equals its final id segment. The `xml`
field overrides only that leaf binding:

- `xml = "@currencyID"` — the leaf is the `currencyID` **attribute** of its
  parent element, not a child element.
- `xml = "$text"` — the leaf is its parent element's **text content**.
- `xml = "SomeOtherName"` — the leaf element is **renamed** (useful when the
  XML name is not a valid TOML key or clashes with a sibling).

Interior segments are taken verbatim from the id unless the interior element
is itself declared as a collection or structural node with an `xml` name — the
way a second node binds an element another node already uses (see
[Structural matching](#structural-matching-one-element-several-nodes)).

A node whose text is a value *and* which has attribute children (a "valued
element") — or whose [codec](#codecs) carries wire attributes — is handled
automatically: the compiler synthesizes a struct with a
text value field plus the attribute fields, all from the ids:

```toml
# <PayableAmount currencyID="EUR">100.00</PayableAmount>
[Invoice.LegalMonetaryTotal.PayableAmount]        # the element text: 100.00
type = "decimal"
canonical_key = "PayableAmount"

[Invoice.LegalMonetaryTotal.PayableAmount.currencyID]
xml = "@currencyID"                               # the attribute: EUR
type = "currency"
canonical_key = "PayableAmountCurrency"
```

---

## Types

`type` is one of the following closed set (lower-case keywords):

| Type | Meaning |
|------|---------|
| `string` | Free text; may be empty after normalization |
| `identifier` | An id; empty/whitespace after normalization counts as missing |
| `date` | A calendar date (`YYYY-MM-DD`). Shape check only: month 01–12, day 01–31 — no calendar arithmetic, so e.g. February 30 passes |
| `datetime` | A date-time (`YYYY-MM-DDThh:mm:ss` plus optional fraction/zone). Shape check only, like `date` |
| `decimal` | A scale-preserving decimal (zero is valid) |
| `currency` | An ISO 4217 currency code |
| `unit_code` | A unit-of-measure code |
| `boolean` | A boolean |
| `collection` | Structural: a repeated item that opens a child scope |

> There is intentionally no `amount` type: an amount and its currency are
> mapped as two separate nodes (`decimal` + `currency`), usually a valued
> element and its attribute as shown above.

---

## Normalization

`normalize = [...]` applies a sequence of compiler-known transforms, in order,
before type validation. Normalization is *not* scripting: unknown operations
are rejected, and **no type is normalized implicitly** — a `currency` is not
upper-cased and an `identifier` is not trimmed unless the node says so.

| Op | Effect |
|----|--------|
| `trim` | Strip leading/trailing whitespace |
| `uppercase` | Upper-case the value |
| `lowercase` | Lower-case the value |
| `empty_as_missing` | Treat an empty (post-trim) value as missing rather than present-but-empty |

```toml
normalize = ["trim", "uppercase"]          # e.g. for a currency code
normalize = ["trim", "empty_as_missing"]   # e.g. for an identifier
```

---

## Multiple values

Whether a scalar node declares `multiple` changes the **shape** of the
synthesized source field:

- **No `multiple`** — the field is strictly single-valued. A document that
  repeats the element fails to deserialize; the repetition is a document
  error, not something to silently collapse.
- **Any `multiple` policy** — the source field becomes a list, and the values
  are collapsed per the policy:

| Policy | Effect |
|--------|--------|
| `error` | Runtime diagnostic error if more than one value is found |
| `first` | Use the first value in source order; warn when more were present |
| `join` | Join all values in source order using `join_with` |

```toml
[Invoice.Note]
type = "string"
canonical_key = "Notes"
multiple = "join"
join_with = "\n"
```

`join_with` is required exactly when the policy is `join` (E040). `multiple`
is only valid on a plain scalar element leaf — not on collections, attributes,
`$text` overrides, or valued containers — and cannot be combined with
`fallbacks`: a multi-valued node collapses its own values, and a fallback
chain on top has no defined order of application (E043). `multiple` on a
node of the wrong shape is E024.

---

## Collections & scopes

A `type = "collection"` node marks a repeated element and opens a **child
scope** for its descendant nodes. Collections nest.

```toml
[InvoiceLine]                  # repeated <InvoiceLine> element
type = "collection"
canonical_key = "InvoiceLines"
required = true

[InvoiceLine.ID]               # a field on each line item
type = "identifier"
canonical_key = "LineId"

[InvoiceLine.Item.Name]        # nested aggregate, inferred from the id
type = "string"
canonical_key = "ItemName"
```

A `required` collection must have at least one item: reading a document
without any, or writing a hub without any, is a `REQUIRED_MISSING` error, like
a missing required scalar.

Scopes matter for two rules:

- A **fallback** must live in the same scope as the referring node (E032):
  root nodes fall back to root nodes, and a collection child falls back only
  to siblings inside the same collection item.
- A **canonical key declared inside a collection** attaches to that
  collection's canonical item — so the enclosing collection node must itself
  have a `canonical_key` (E011).

---

## Structural matching: one element, several nodes

Formats often reuse one element for several business terms and tell the
occurrences apart by a child value: CII's `AdditionalReferencedDocument` is a
tender reference (`TypeCode` 50), an invoiced object (130) or a supporting
document (916); UBL's seller `PartyTaxScheme` is the VAT id (`TaxScheme/ID`
`VAT`) or the tax registration (`FC`). A `match` selector binds a node to just
the occurrences whose child values equal the selector's, and `xml` on a
collection or structural node lets a second node — under an id of its own —
bind the same physical element:

```toml
# <ram:AdditionalReferencedDocument><ram:IssuerAssignedID>…</ram:IssuerAssignedID>
#   <ram:TypeCode>916</ram:TypeCode>…</ram:AdditionalReferencedDocument>
[Agreement.AdditionalReferencedDocument]         # BG-24: the rest, written as 916
type = "collection"
canonical_key = "SupportingDocuments"

[Agreement.AdditionalReferencedDocument.IssuerAssignedID]
type = "identifier"
canonical_key = "SupportingDocumentReference"

[Agreement.AdditionalReferencedDocument.TypeCode] # the discriminator, in sequence position
type = "string"
constant = "916"

[Agreement.TenderOrLotReferencedDocument]        # BT-17: the one with TypeCode 50
xml = "AdditionalReferencedDocument"
match = { "TypeCode" = "50" }

[Agreement.TenderOrLotReferencedDocument.IssuerAssignedID]
type = "identifier"
canonical_key = "TenderOrLotReference"
```

Every node bound to the element — the one keeping the element's own name and
every `xml` alias — is a **logical node** of that **physical element**. The
compiler synthesizes the element once, as one repeated field whose item struct
is the union of all logical nodes' children, plus one field per logical node.
The generated reader first sorts every occurrence into the logical node whose
selector it satisfies (a structural node keeps the first and warns
`MATCH_MULTIPLE` on more; a logical node without a selector takes what no
selector matched; an occurrence nothing claims is ignored), then maps as usual.
The writer maps into the logical fields and finally merges them back into the
element in declaration order, **writing each selector's values as
discriminators** — so a node needs no `constant` for its own discriminator; a
`constant` is only how the selector-less default node pins its code (`916`
above).

Rules:

- `match` keys are child paths inside the element — an element (`TypeCode`), a
  nested one (`TaxScheme.ID`), an attribute (`@schemeID`, `ID.@schemeID`) or
  `$text`; values are compared trimmed and exactly. Every key must name a
  single scalar declared beneath one of the element's logical nodes (E092);
  declare the discriminator as a node (a helper is enough) under the *first*
  logical node, in its schema position, because the shared item struct orders
  each child by its first declaration.
- The logical nodes of one element must be **disjoint**: two selectors must
  share a key with different values, and at most one node may have no
  selector (E090). `match` belongs on a collection or structural node, never
  a scalar (E091). The logical nodes must agree on `ns` (E024 otherwise), and
  a child element declared under several of them must have the same shape.
- A structural node with a selector is single-valued: its children are
  ordinary scalars of the enclosing scope (`TenderOrLotReference` above is a
  root key). A collection with a selector is a collection like any other.
- Under [inheritance](#inheritance) a CIUS restates only the selector
  ([peppol.toml](peppol.toml) changes the tax-registration scheme id from
  `FC` to `TAX`); the `xml` alias and the children are inherited.

---

## Canonical keys & the hub

`canonical_key` is how a node connects to the shared model. The hub
(`MainKey`) is **derived** as the union of every spoke's canonical keys —
there is no hand-maintained canonical schema. Two formats round-trip through
the hub precisely because they share canonical keys.

Rules, all enforced at build time:

- **Cross-format type agreement (E010).** When two spokes declare the same
  `canonical_key`, they must declare it with the same type (and the same
  collection-ness). That's what keeps UBL ⇄ XRechnung lossless on the keys
  they share.
- **One declaration per key per spoke (E013).** Within one spoke, a canonical
  `(scope, key)` may be mapped by only one primary node. If two source paths
  can carry the value, pick one primary and express read priority with
  `fallbacks`, or mirror the value with `clone_of` — never two primaries.
- **Keys are PascalCase identifiers (E014).** An upper-case ASCII letter,
  then ASCII letters and digits (not `Self`): the key names a hub field and,
  for a collection, its `<Key>Item` struct.
- **No orphan keys inside anonymous collections (E011).** A key inside a
  collection needs the collection itself to be keyed.
- **No generated-name collisions (E012).** Two keys that collapse to the same
  generated Rust field name (e.g. `FooBar` and `Foo_bar`, both `foo_bar`), or a
  collection key reused in two different scopes, would break the generated
  hub — rename one. So would a source struct that takes a generated hub
  type's name (an element path camel-casing to `InvoiceLinesItem` beside the
  `InvoiceLines` collection key).

A node with **no** `canonical_key` is a *helper* node — it carries no hub
value itself and exists only to be referenced as a fallback.

Use `krab-cli --keys` to browse the current hub vocabulary, and
`krab-cli --keys <format>` to see which keys a spoke already covers and which
hub keys it does not yet map.

---

## Fallbacks

`fallbacks` lists other node ids to try, in order, when this node's value is
missing:

```toml
[Invoice.IssueDate]
type = "date"
canonical_key = "IssueDate"
fallbacks = ["Invoice.TaxPointDate"]
```

Everything about a fallback is checked at compile time:

- the target must exist and not be disabled (E030);
- the target's type must be compatible (E031) — `string` and `identifier` are
  interchangeable, every other type only falls back to itself;
- the target must share the referring node's scope (E032);
- fallback chains must not form a cycle (E033).

Fallback targets are often helper nodes: map the preferred source path as the
primary, the alternative path as a keyless helper, and chain them.

---

## Constants: pinning write-side values

`constant` fixes the value a spoke **writes**, regardless of what the hub
carries. The read side is untouched.

A constant is written where the schema would otherwise see a hole, never on
its own: its *owner* — the deepest interior element on its path that other
mapped nodes also write into — must be non-empty. So `TaxScheme/ID = "VAT"`
under `PartyTaxScheme` appears exactly when the party has a
`PartyTaxScheme/CompanyID`, and `TypeCode = "VAT"` under CII's
`ApplicableTradeTax` appears with each breakdown. A constant that shares no
interior element with real content (`UBLVersionID` at the root) is written
unconditionally; inside a collection, on every non-empty item. So is a
constant whose owner is always written anyway (a structural node with
`required = true`, such as Factur-X's `ExchangedDocumentContext`): an owner
that is never empty needs no content to justify the constant.

```toml
# Written only when PartyTaxScheme carries a CompanyID.
[Invoice.AccountingSupplierParty.Party.PartyTaxScheme.TaxScheme.ID]
type = "identifier"
constant = "VAT"
```

The complement, an element the schema makes mandatory even when empty, is a
**structural node with `required = true`**: the writer always materializes it.

```toml
# CII: emitted as <ram:ApplicableHeaderTradeDelivery/> even with no delivery data.
[CrossIndustryInvoice.SupplyChainTradeTransaction.ApplicableHeaderTradeDelivery]
required = true
```

This is how a spoke pins spec-mandated values — a CIUS `CustomizationID` URN,
a `UBLVersionID` — without leaking another format's value into its output:

```toml
# Read side: the document's CustomizationID still fills SpecificationId in the
# hub. Write side: this spoke always emits its own URN, never the source's.
[Invoice.CustomizationID]
type = "identifier"
canonical_key = "SpecificationId"
constant = "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0"

# Write-only: no canonical_key, so reading ignores it entirely; writing always
# emits the literal.
[Invoice.UBLVersionID]
type = "identifier"
constant = "2.1"
```

Rules:

- The literal must parse under the node's `type` (E061); shape checks only —
  malformed values fail the build instead of surfacing in emitted documents.
- Not valid on a collection node (E060).
- Cannot be combined with `fallbacks`, `multiple` or `codec` (E062): the
  constant is emitted verbatim on write, so read-side collapse features don't
  apply. `normalize` is fine: it shapes what is *read* from the node, the
  constant is what is *written*.

---

## Clones: one value, several places

Some formats store the same value in several places. `clone_of` names a
canonical key declared by a primary node **in the same scope**; the clone
node then mirrors it:

```toml
[Invoice.ID]
type = "identifier"
canonical_key = "InvoiceNumber"

# This format repeats the invoice number here; keep the copies in sync.
[Invoice.OrderReference.SalesOrderID]
type = "identifier"
clone_of = "InvoiceNumber"
```

- **Write side:** the writer fans the key's hub value out to the clone's path
  too.
- **Read side:** the clone never fills the hub. It only checks the document's
  copy against the canonical value and emits a `CLONE_MISMATCH` warning when
  the copies disagree.

A clone may reach outside its own scope: `$parent.Key` names a key of the
enclosing collection's scope, `$root.Key` a key of the invoice root. This is
how every amount gets the document currency UBL's schema demands on it:

```toml
[InvoiceLine.LineExtensionAmount.currencyID]
xml = "@currencyID"
type = "currency"
clone_of = "$root.DocumentCurrency"
```

Rules: the derivation is `Key`, `$parent.Key` or `$root.Key` — anything else,
or `$parent` at the root, is E093; the target key must be declared by a
primary node in the referenced scope (E071) with the same type (E072). A clone
is *only* a mirror — it cannot also declare `canonical_key`, `constant`,
`fallbacks` or `multiple`, and a collection cannot be a clone (E070).

---

## Codecs

The hub stores dates as ISO `YYYY-MM-DD`, date-times as ISO
`YYYY-MM-DDThh:mm:ss`, and booleans natively. Formats write them differently —
CII needs `<udt:DateTimeString format="102">20260718</udt:DateTimeString>` — so
the translation is declared once as a **codec** in
[`config/codecs/*.toml`](../codecs/) and named by the node:

```toml
# config/codecs/dates.toml
[codec.cii-date-102]
for_type = "date"
lexical  = "YYYYMMDD"
wire     = { "@format" = "102" }

# config/mappings/cii.toml
[CrossIndustryInvoice.ExchangedDocument.IssueDateTime.DateTimeString]
type = "date"
canonical_key = "IssueDate"
codec = "cii-date-102"
```

On read the lexical form is decoded into the canonical form (a value that does
not match is a `CODEC_INVALID` error diagnostic; a wire attribute present with
a different value is a `CODEC_WIRE_MISMATCH` warning). On write the canonical
value is encoded and the `wire` attributes are emitted on the same element.
Codecs are loaded before the mappings and shared by all of them.

A codec declares exactly one kind, checked when the codec file loads:

| Kind | `for_type` | Write | Read |
|---|---|---|---|
| `lexical` | `date`, `datetime`, `boolean` | the canonical value in the pattern | the pattern decoded |
| `values = [[canonical, wire], …]` | `string`, `identifier`, `currency`, `unit_code` | the first pair's wire code for the hub value; a hub value without a pair is `CODEC_INVALID`; an empty wire code writes nothing | the first pair's canonical code for the wire value; several wire codes may read as one canonical code |
| `fraction_digits = [min, max]` | `decimal` | zero-padded to `min` fraction digits, trailing zeros trimmed down to it; a value needing more than `max` is `CODEC_INVALID` (never rounded) | as a decimal |
| `charset = "latin-1"` | `string`, `identifier` | the text in ISO 8859-1, typographic punctuation transliterated (`—` → `-`, `€` → `EUR`); any other character outside it is `CODEC_INVALID` | as is |
| `digits = n` | `string`, `identifier` | the value when it is exactly `n` ASCII digits, else `CODEC_INVALID` (never padded or cut) | as is |
| `split = { at = n, into = [head, tail] }` | `string`, `identifier` | the first `n` characters into child element `head`, the rest into `tail` | the two joined |

The `lexical` pattern language is small:

| `for_type` | `lexical` | Rules |
|---|---|---|
| `date` | tokens `YYYY` `MM` `DD`, separators `-` `.` `/` | each token exactly once, no time tokens |
| `datetime` | tokens `YYYY` `MM` `DD` `hh` `mm` `ss`, separators `-` `.` `/` `:` `T` space | `ss` optional, the rest exactly once |
| `boolean` | `yes\|no` | the two literals, distinct and non-empty |

A code table is how a format's own code list meets EN 16931's — FatturaPA's
`TipoDocumento` for the UNTDID 1001 invoice type code:

```toml
[codec.fatturapa-tipo-documento]
for_type = "string"
values = [
    ["380", "TD01"],   # commercial invoice → fattura
    ["381", "TD04"],   # credit note → nota di credito
    ["380", "TD06"],   # read-only: parcella reads as a commercial invoice
]
```

Whatever cannot be represented is refused, never approximated: a write that
fails a codec reports `CODEC_INVALID` naming the node and value, and the
transform yields no document. A value a codec changes on the way (a
transliteration, a many-to-one code) is reported as *recoded* by the
[conformance check](#schema-conformance).

Codec ids are globally unique and stable (`[a-zA-Z0-9_-]+`): changing a codec's
behaviour means adding a new id. Rules on the node: the codec must exist (E084)
and its `for_type` must equal the node's `type` (E085); a codec's wire
attribute must not collide with an attribute node declared on the same element
(E087); `constant` and `codec` are mutually exclusive (E062). A codec with wire
attributes turns its element into a valued container (text plus attributes)
exactly as an attribute child would. A `split` codec turns its element into a
container of the two named children and takes no `wire` attributes.

---

## Read defaults

`default` names the value a node reads into its canonical key when the
document lacks the element — for a format that expresses the common case by
omission. FatturaPA writes no `Natura` for a standard-rated amount, so its
mapping reads an absent `Natura` as EN 16931 category `S` (and the
`fatturapa-natura` code table writes `S` as nothing):

```toml
[FatturaElettronica.FatturaElettronicaBody.DatiBeniServizi.DatiRiepilogo.Natura]
type = "string"
canonical_key = "VatCategoryCode"
codec = "fatturapa-natura"
default = "S"
```

A default is read-side only; the writer never emits it. It fills the node's
own key, so it needs a `canonical_key` and is not valid on a collection or a
`clone_of` node (E064), and its literal must parse under the node's `type`
(E063). Inside a collection it applies to each item read.

---

## Derivations: EN 16931 calculation rules

EN 16931 defines its document totals by calculation rules (BR-CO-10 to
BR-CO-16), and some formats leave out what the others require: FatturaPA
states no sum of line net amounts, no total without VAT. Rather than fail
every transform out of such a format, the engine computes, before writing,
each canonical value [`config/derivations.toml`](../derivations.toml)
defines and the hub lacks. A value the source carries is never replaced, and
each derived value is reported as a `VALUE_DERIVED` info diagnostic.

```toml
# BR-CO-10: sum of invoice line net amounts.
[[derive]]
key = "SumOfInvoiceLineNetAmount"
rule = "BR-CO-10"
sum = "InvoiceLines/LineNetAmount"

# BR-CO-11: only the allowances (ChargeIndicator false) are summed.
[[derive]]
key = "SumOfAllowancesDocumentLevel"
rule = "BR-CO-11"
sum = "DocumentAllowanceCharges/AllowanceChargeAmount"
where = { ChargeIndicator = "false" }

# BR-CO-13: total without VAT = lines − allowances + charges.
[[derive]]
key = "InvoiceTotalWithoutVat"
rule = "BR-CO-13"
add = ["SumOfInvoiceLineNetAmount", "SumOfChargesDocumentLevel"]
subtract = ["SumOfAllowancesDocumentLevel"]

# BR-42: an allowance with neither reason nor reason code gets code 95.
[[derive]]
key = "InvoiceLines/LineAllowanceCharges/LineAllowanceChargeReasonCode"
rule = "BR-42"
value = "95"
where = { LineChargeIndicator = "false" }
unless = ["LineAllowanceChargeReason"]
```

| Field | Meaning |
|-------|---------|
| `key` | The canonical key derived: a root key, or for `value` also an item key by its label (`Collection/…/Key`) |
| `rule` | The EN 16931 rule it implements, for reports |
| `sum` | `Collection/Key`: the sum of a `decimal` item key over a root collection's items (those matching `where`); no contributing item derives nothing |
| `add` / `subtract` | Root `decimal` keys: the first `add` operand must be present, any other absent operand counts as zero |
| `requires` | Root keys that must be present for the rule to apply |
| `skip_zero` | Derive nothing when the result is zero (default `false`) |
| `value` | A literal set on the target (on every item matching `where`) when neither it nor any `unless` key is present |
| `where` | `{ Key = "literal" }`: an item filter on another key of the item |
| `unless` | Keys whose presence (on the item) suppresses a `value` |

Rules apply in file order, so a rule may use a total an earlier rule derived.
The build checks the file against the hub: a computed target must be a root
`decimal` key computed by one rule only (several `value` rules may fill one
key under different filters) (E110); a `sum` must name a `decimal` key of a
root collection (E111); a `where` key must exist and its literal fit its type
(E112); an operand must be a root `decimal` key, and a `requires` operand must
not be derived by this or a later rule (E113); a `value` must fit its key's
type (E114).

[`krab-cli --analyze`](#checking-your-mapping) counts a key a target requires
as available when a derivation can supply it.

---

## `required` and the transformation contract

`required = true` on a mapped node is a promise about the *written* document:
it will carry the node's value. The compiler reads that as "the node has a
deterministic **write route**" — one of:

- its **hub key**, which the source of a transform must map;
- a **`constant`**, always available;
- a **`clone_of`**, resolved to the key it mirrors (which the source must map);
- for a structural node, the element itself, always materialized (see
  [Namespaces](#namespaces)).

The route is checked at three points. At build time, a key a spoke requires
from the hub that **no other spoke maps** is W095: no transform into that
spoke, except from itself, could ever supply it. Per transform, `krab-cli
--analyze <source> <target>` reports every required route the source cannot
feed. At runtime, a document that still arrives without the value — or a
required collection without items — is a `REQUIRED_MISSING` error, on read
and on write alike. Making a node required never relaxes any of these; it only
adds the static checks.

Everything a transform analysis needs is a spoke's **transformation
contract**, embedded in the generated registry as `Spoke::contract()`:

| Part | Contents |
|------|----------|
| `keys` | every canonical key the spoke maps, as a scope-qualified label (`InvoiceLines/LineId`) with its type, codec and write-side pin (`constant` on a keyed node) |
| `required` | every `required` node's write route (`Hub(label)`, `Constant(value)`, `Clone(label)`) |
| `collapses` | nodes whose `multiple = "first" \| "join"` collapses several source values into one |
| `selectors` | the `match` selectors, and whether each node keeps one occurrence or all |

Two contracts determine a pair: `--analyze` (and `GET /analyze`) compares them
and reports missing required routes and type clashes (blocking: the output is
partial), dropped keys (lossy), and — as information — required routes fed by
a key the source maps but does not itself require (a document lacking it is
`REQUIRED_MISSING` at runtime), pins, recodes (a key read and written through
different codecs) and the source's declared collapses. `--deny-lossy` turns
anything but a lossless verdict into exit code 65, for CI gates; the verdict is
structural, so the informational findings never trip it.

---

## Inheritance

`[meta].inherits` names a parent mapping id (its `source_model`, or
`doc_format:format_version`). At build time the inheritance chain is resolved
ancestor-first, so the child starts from the parent's full node set and only
declares its deltas:

- Re-declaring a node id **merges over the base node**: fields you set win,
  fields you omit keep the base's values, so a CIUS states only its delta.
  `replace = true` opts back into whole-node replacement (omitted fields then
  take their defaults). Either way the node keeps the base's declaration
  position, so the element stays where the base emits it.
- Nodes the child adds are emitted after every base node, in the child's own
  declaration order (see [Declaration order is schema order](#declaration-order-is-schema-order)).
- The namespace entries of `[meta]` (`root_ns`, `namespaces`, `ns_defaults`)
  and `[meta.schema]` are inherited whole when the child omits them;
  `[[meta.samples]]` never are.
- `disabled = true` on a node removes it from the effective mapping.
- `disabled = true` in `[meta]` makes the mapping itself **inherit-only**: it
  can be inherited from but emits no spoke (see [cii.toml](cii.toml), which
  exists only to be specialized by [facturx.toml](facturx.toml)).

Missing parents and inheritance cycles fail the build.

[xrechnung.toml](xrechnung.toml) is the reference CIUS: its own `[meta]`
identity, a `detect` marker, and a one-line override making `CustomizationID`
required — everything else (~150 nodes) folds in from [ubl.toml](ubl.toml)
unchanged:

```toml
[meta]
doc_format = "xrechnung-invoice"
format_version = "3.0.2"
mapping_version = "2.0"
source_model = "xrechnung-invoice:3.0.2"
canonical_model = "canonical-invoice:1.0"
root = "Invoice"
inherits = "ubl-invoice:2.1"
detect = ["xrechnung"]

# Merges over the base node: only the delta is stated.
[Invoice.CustomizationID]
required = true
```

---

## Auto-detection

When a document is valid under more than one format (an XRechnung is also
valid UBL), `[meta].detect` markers break the tie. Markers are matched
**case-insensitively** as substrings of the document's `CustomizationID`
(EN 16931 BT-24). A format whose marker is present wins over a base format
that declares none.

```toml
[meta]
detect = ["xrechnung"]
```

A base format (plain UBL) simply leaves `detect` empty and acts as the
fallback when no marker matches.

---

## Schema conformance

A mapping can declare the schema its format is defined by and the documents
that prove the mapping right. The build embeds both in the registry
(`Spoke::schema`, `Spoke::samples`) and the checks are derived from them, so
no output test has to be written or kept in step with the TOML by hand.

```toml
[meta.schema]
xsd = "testfiles/xsd/ubl-2.1/maindoc/UBL-Invoice-2.1.xsd"
refuses = ["testfiles/en16931-full-fatturapa.xml"]

[[meta.samples]]
file = "testfiles/xrechnung-3.0.2-beispiel.xml"
source = "xrechnung-invoice"     # optional: the spoke that reads it
```

| Field | Required | Purpose |
|-------|----------|---------|
| `schema.xsd` | ✅ | The root XSD |
| `schema.catalog` | — | An XML catalog resolving the schema's remote imports offline (passed as `XML_CATALOG_FILES`) |
| `schema.known_gaps` | — | Substring patterns of the schema errors the spoke's output is documented to still produce |
| `schema.refuses` | — | Samples the spoke is documented to refuse: their data cannot be represented in its format |
| `samples.file` | ✅ | A sample document |
| `samples.source` | — | The spoke that reads the sample, as a mapping id or a bare `doc_format`; default: the declaring mapping |

Every path is relative to the workspace root (the parent of `config/`). A
path that names no file fails the build (E100), and so does a sample no
spoke reads: a `source` naming no emitted spoke, or a sample on an
inherit-only base without a `source` (E101).

`[meta.schema]` is inherited like the namespace entries, so a CIUS validates
against the schema its base declares; a child's own table replaces the
parent's whole, which is how XRechnung and Peppol add their own refusals to
the UBL 2.1 schema [ubl.toml](ubl.toml) declares. Samples are
never inherited: each runs once, read by its spoke.

### What gets verified

For every sample `D`, read by its spoke `R`, and every spoke `S` with a
`[meta.schema]`:

1. **Sample validity** — `D` validates against `R`'s XSD, and `R` reads it
   without error diagnostics. A broken
   fixture fails here, at the fixture; its pairs are not run.
2. **Emitted validity** — `D` read by `R` and written by `S` validates against
   `S`'s XSD, up to `S`'s `known_gaps`.
   Every schema error must match a gap pattern, and every pattern must still
   match an error of some document `S` wrote: a stale pattern fails, so the
   lists only ever shrink. A sample has no gaps; it must validate outright.
   When `D` is in `S`'s `refuses`, the write must instead end in error
   diagnostics (`REQUIRED_MISSING` naming what the sample lacks,
   `CODEC_INVALID` naming what the format cannot hold), which are reported;
   a declared refusal that now writes cleanly fails as stale.
3. **Round trip** — the document `S` wrote, read back by `S`, yields the
   same hub values as reading `D`, for every canonical key `S` covers.
   Keys `D` carries that `S` does not cover (dropped), keys `S` pins to a
   [constant](#constants-pinning-write-side-values) on write, values a codec
   recodes, and values [derived](#derivations-en-16931-calculation-rules) on
   write are reported, never failed.

Validation runs `xmllint --noout --nonet --schema <xsd>`. Without `xmllint`
on `PATH` the schema checks are skipped with a notice and the round trips
still run; CI installs `libxml2-utils`, so there they always run.

The checks run in `cargo test` (`crates/einvoice-interfaces/tests/xsd_validation.rs`)
and on demand, with the report, as `krab-cli --check [ROOT]`:

```text
sample testfiles/en16931-full-fatturapa.xml (read by fatturapa:1.2.2)
  valid against testfiles/xsd/fatturapa-1.2.2/Schema_del_file_xml_FatturaPA_v1.2.2.xsd
  -> facturx-invoice:1.0: ok (valid, 45 key(s) round-trip)
       derived: InvoiceTotalWithoutVat: written as ["200.00"]
       …
  -> xrechnung-invoice:3.0.2: ok (refused, as declared)
       refused: [REQUIRED_MISSING] Invoice.BuyerReference: required value is missing
       …
```

The mapping stays the source of truth for element order and cardinalities;
the schema is the oracle that checks them. Business rules (EN 16931
Schematron and the CIUS rule sets) are not run by this check; validate with
them as your own toolchain provides.

---

## A complete example

A minimal but complete spoke:

```toml
[meta]
doc_format = "ubl-invoice"
format_version = "2.1"
mapping_version = "1.0"
canonical_model = "canonical-invoice:1.0"
root = "Invoice"

[Invoice.ID]
type = "identifier"
canonical_key = "InvoiceNumber"
required = true
normalize = ["trim", "empty_as_missing"]

[Invoice.IssueDate]
type = "date"
canonical_key = "IssueDate"
fallbacks = ["Invoice.TaxPointDate"]

# Fallback-only helper: no canonical_key.
[Invoice.TaxPointDate]
type = "date"

[Invoice.DocumentCurrencyCode]
type = "currency"
canonical_key = "DocumentCurrency"
normalize = ["trim", "uppercase"]

# A valued element: its text is the amount, currencyID is its attribute.
[Invoice.LegalMonetaryTotal.PayableAmount]
type = "decimal"
canonical_key = "PayableAmount"

[Invoice.LegalMonetaryTotal.PayableAmount.currencyID]
xml = "@currencyID"
type = "currency"
canonical_key = "PayableAmountCurrency"

# A required collection of invoice lines (at least one item).
[InvoiceLine]
type = "collection"
canonical_key = "InvoiceLines"
required = true

[InvoiceLine.ID]
type = "identifier"
canonical_key = "LineId"

[InvoiceLine.InvoicedQuantity]
type = "decimal"
canonical_key = "Quantity"

[InvoiceLine.Item.Name]
type = "string"
canonical_key = "ItemName"
```

See [ubl.toml](ubl.toml) for the full reference spoke with per-node comments.

---

## Checking your mapping

The DSL crate ships an `xtask` dev CLI that loads the mappings through the
exact same loader and compiler the build uses — what `check` accepts, the
build accepts:

```bash
# Compile every codec and mapping and print all diagnostics
cargo run -p einvoice-dsl -- check config

# Print a canonical coverage matrix and gap report
cargo run -p einvoice-dsl -- report config
```

`check` also verifies the files your `[meta.schema]` and `[[meta.samples]]`
declare exist (E100), that every sample has a reader (E101), and that
`config/derivations.toml` fits the hub (E110–E114).

Once it builds, the CLI offers two static authoring aids (no input document
needed), and the schema verdict on your declared samples:

```bash
krab-cli --keys <your-format>    # covered vs. unused canonical keys
krab-cli --analyze <your-format> # which conversions lose data, and what
krab-cli --check                 # XSD validity and round trips (see Schema conformance)
```

Validation reports **every** problem in one run, never just the first error.

---

## Diagnostic code reference

| Code | Meaning |
|------|---------|
| `E001` | Unknown field in `[meta]` or a node table |
| `E002` | Active node missing its `type` |
| `E010` | Canonical key declared with conflicting types across spokes |
| `E011` | Canonical key inside a collection that has no `canonical_key` itself |
| `E012` | Two canonical keys collide in generated code (same Rust field name, or one collection key in two scopes), or a source struct takes a generated hub type's name |
| `E013` | Same canonical key mapped by two primary nodes in one spoke (use `fallbacks` or `clone_of`) |
| `E014` | `canonical_key` (or the key a `clone_of` mirrors) is not a PascalCase identifier |
| `E020` | `[meta].source_model` disagrees with the synthesized model id |
| `E021` | Node id does not resolve to a source path |
| `E022` | Collection node whose path is not a repeated field |
| `E023` | Scalar node whose path resolves to a struct, not a leaf |
| `E024` | Incompatible bindings of one element: a leaf, attribute, valued container or collection shape conflict; `multiple` on an attribute, `$text`, collection or valued container; logical nodes of one element disagreeing on `ns` |
| `E025` | Two nodes bind one element, attribute or element text (e.g. `[ID]` and `[Invoice.ID]`, or a node renamed onto another's element with `xml`) |
| `E026` | An id segment or `xml` binding is not an XML name, or `[meta].root` is not also a plain Rust type name |
| `E030` | Fallback target does not exist or is disabled |
| `E031` | Fallback target type incompatible with the primary |
| `E032` | Fallback target in a different scope |
| `E033` | Fallback reference cycle |
| `E040` | `multiple = "join"` without `join_with`, or `join_with` without join |
| `E043` | `multiple` combined with `fallbacks` |
| `E060` | `constant` on a collection node |
| `E061` | `constant` literal does not parse under the node's `type` |
| `E062` | `constant` combined with `fallbacks`, `multiple` or `codec` |
| `E063` | `default` literal does not parse under the node's `type` |
| `E064` | `default` on a node without a `canonical_key`, a collection, or a `clone_of` node |
| `E070` | `clone_of` on a collection, or combined with `canonical_key`, `constant`, `fallbacks` or `multiple` |
| `E071` | `clone_of` target key not declared by a primary node in the referenced scope |
| `E072` | `clone_of` node's `type` differs from its target's |
| `E080` | Namespace prefix used by `root_ns`, `ns_defaults`, or a node's `ns` but not declared in `[meta.namespaces]` |
| `E081` | `ns` on an attribute or `$text` leaf (never prefixed) |
| `E083` | Structural node that names no element (nothing beneath it, or an attribute/`$text` binding) or names the root (use `root_ns`) |
| `E084` | Unknown codec id |
| `E085` | `codec` on a collection, or codec `for_type` differs from the node's `type` |
| `E087` | Codec wire attribute collides with an attribute node on the same element |
| `E090` | Two logical nodes of one element overlap: identical or non-disjoint `match` selectors, or two nodes without one |
| `E091` | `match` on a scalar node |
| `E092` | `match` key does not name a single scalar declared beneath the element's logical nodes (or the selector is empty) |
| `E093` | Malformed `clone_of` derivation (`$sibling.Key`, `$root.A.B`), or `$parent` at root scope |
| `W095` | A `required` node needs a hub key no other spoke maps (warning): no transform into this spoke, except from itself, can supply it |
| `E100` | A `[meta.schema]` (`xsd`, `catalog`, `refuses`) or `[[meta.samples]]` path is absolute or names no file under the workspace root |
| `E101` | A sample has no reader: its `source` names no emitted spoke, or it is declared on an inherit-only base without a `source` |
| `E110` | A derivation target is no fitting canonical key (a computed total: a root `decimal`; a `value`: a scalar key), or a total is computed by more than one rule |
| `E111` | A `sum` names no `decimal` key of a root collection |
| `E112` | A `where` key is unknown, or its literal does not fit the key's type |
| `E113` | An `add` / `subtract` / `requires` operand is no root `decimal` key, or a required operand is derived by this or a later rule |
| `E114` | A `value` literal does not fit the target key's type |

Runtime (per-document) diagnostics — missing required values, type validation
failures, taken fallbacks, `CLONE_MISMATCH`, `CODEC_INVALID`,
`CODEC_WIRE_MISMATCH`, `MATCH_MULTIPLE`, `VALUE_DERIVED` — are reported with severity and a
source-node reference when a document is transformed; they never silently
vanish.
