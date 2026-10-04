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
- [Canonical keys & the hub](#canonical-keys--the-hub)
- [Fallbacks](#fallbacks)
- [Constants: pinning write-side values](#constants-pinning-write-side-values)
- [Clones: one value, several places](#clones-one-value-several-places)
- [Codecs](#codecs)
- [Adapters (deprecated)](#adapters-deprecated)
- [Inheritance](#inheritance)
- [Auto-detection](#auto-detection)
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
5. **Validation** — paths, fallbacks, scopes, constants, clones, and adapters
   are checked; every problem is reported (never just the first).
6. **Codegen** — the `read`/`write` mappers and the format registry (the
   `Spoke` enum, display names, detection markers) are emitted as Rust.

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
  parent's children stay contiguous wherever they are declared.
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
`description`) and no `type`:

```toml
# CII: the root's three children are rsm:, everything beneath them ram:.
[CrossIndustryInvoice.ExchangedDocument]
ns = "rsm"
```

A structural node is consumed by the compiler when it creates that interior
element; it never maps a value, so it is an error for it to name nothing (no
typed node beneath it) or the root (whose prefix is `root_ns`) — E083. A
`type`-less table with any *mapping* field (`canonical_key`, `xml`, `required`,
…) is still E002.

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

`source_model` is also an assertion: if it disagrees with the synthesized
model's id, the build fails (E020). Duplicate mapping ids or slugs across
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
| **Structural** | `ns` only, no `type` | nothing | names an interior element's prefix (see [Namespaces](#namespaces)) |

### Node fields reference

| Field | Type | Meaning |
|-------|------|---------|
| `type` | string | Value type (see [Types](#types)). Required for active nodes (E002). |
| `canonical_key` | string | Target field in the canonical hub. Omit for a helper node. |
| `xml` | string | Leaf binding override (see [below](#the-xml-field-attributes-text-renames)). |
| `required` | bool | Whether the value must be present (default `false`). |
| `normalize` | array | String transforms, applied in order (see [Normalization](#normalization)). |
| `fallbacks` | array | Other node ids to try, in order, when this node is missing (see [Fallbacks](#fallbacks)). |
| `multiple` | string | Policy for repeated scalar values (see [Multiple values](#multiple-values)). |
| `join_with` | string | Separator — required iff `multiple = "join"` (E040). |
| `min_items` | int | Minimum item count for a `collection` node (E041 elsewhere). |
| `constant` | string | Fixed write-side literal (see [Constants](#constants-pinning-write-side-values)). |
| `clone_of` | string | Canonical key this node mirrors (see [Clones](#clones-one-value-several-places)). |
| `codec` | string | Id of a shared lexical codec for a `date`, `datetime` or `boolean` node (see [Codecs](#codecs)). |
| `adapter` | string | *Deprecated* (W050): name of a compiler-known value adapter (see [Adapters](#adapters-deprecated)). |
| `description` | string | Human note, reports only. |
| `disabled` | bool | Remove this node from the effective mapping (useful with [inheritance](#inheritance)). |
| `ns` | string | Namespace prefix of this node's own element on write (see [Namespaces](#namespaces)). Alone on a `type`-less table it makes a structural node. |

Any other field is rejected (E001) — typos never silently no-op.

### The `xml` field: attributes, text, renames

By default a leaf's XML local name equals its final id segment. The `xml`
field overrides only that leaf binding:

- `xml = "@currencyID"` — the leaf is the `currencyID` **attribute** of its
  parent element, not a child element.
- `xml = "$text"` — the leaf is its parent element's **text content**.
- `xml = "SomeOtherName"` — the leaf element is **renamed** (useful when the
  XML name is not a valid TOML key or clashes with a sibling).

Interior segments are always taken verbatim from the id and cannot be renamed.

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
chain on top has no defined order of application (E043).

---

## Collections & scopes

A `type = "collection"` node marks a repeated element and opens a **child
scope** for its descendant nodes. Collections nest.

```toml
[InvoiceLine]                  # repeated <InvoiceLine> element
type = "collection"
canonical_key = "InvoiceLines"
required = true
min_items = 1

[InvoiceLine.ID]               # a field on each line item
type = "identifier"
canonical_key = "LineId"

[InvoiceLine.Item.Name]        # nested aggregate, inferred from the id
type = "string"
canonical_key = "ItemName"
```

The effective minimum item count is `min_items` when declared, else `1` when
the collection is `required`, else `0`. `min_items` is only valid on a
collection node (E041).

Scopes matter for two rules:

- A **fallback** must live in the same scope as the referring node (E032):
  root nodes fall back to root nodes, and a collection child falls back only
  to siblings inside the same collection item.
- A **canonical key declared inside a collection** attaches to that
  collection's canonical item — so the enclosing collection node must itself
  have a `canonical_key` (E011).

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
- **No orphan keys inside anonymous collections (E011).** A key inside a
  collection needs the collection itself to be keyed.
- **No generated-name collisions (E012).** Two keys that collapse to the same
  generated Rust field name (e.g. `InvoiceId` and `INVOICE_ID`), or a
  collection key reused in two different scopes, would break the generated
  hub — rename one.

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
- Cannot be combined with `fallbacks`, `multiple`, `adapter`, `normalize`, or
  `codec` (E062): the constant is emitted verbatim on write, so read-side
  collapse and transform features don't apply.

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

Rules: the target key must be declared by a primary node in the same scope
(E071) with the same type (E072). A clone is *only* a mirror — it cannot also
declare `canonical_key`, `constant`, `fallbacks`, `multiple`, or `adapter`,
and a collection cannot be a clone (E070).

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

The pattern language is small and checked when the codec file loads:

| `for_type` | `lexical` | Rules |
|---|---|---|
| `date` | tokens `YYYY` `MM` `DD`, separators `-` `.` `/` | each token exactly once, no time tokens |
| `datetime` | tokens `YYYY` `MM` `DD` `hh` `mm` `ss`, separators `-` `.` `/` `:` `T` space | `ss` optional, the rest exactly once |
| `boolean` | `yes\|no` | the two literals, distinct and non-empty |

Codec ids are globally unique and stable (`[a-zA-Z0-9_-]+`): changing a codec's
behaviour means adding a new id. Rules on the node: the codec must exist (E084)
and its `for_type` must equal the node's `type` (E085); a codec's wire
attribute must not collide with an attribute node declared on the same element
(E087); `constant` and `codec` are mutually exclusive (E062). A codec with wire
attributes turns its element into a valued container (text plus attributes)
exactly as an attribute child would.

---

## Adapters (deprecated)

`adapter` names a compiler-known value transformation implemented in the
runtime crate. The set is closed; an unknown name is a build error (E050), and
**every use is a deprecation warning (W050)**: adapters are superseded by
`normalize` for string operations and by [codecs](#codecs) for lexical forms,
and will be removed.

Currently known:

| Adapter | Effect |
|---------|--------|
| `uppercase_currency` | Upper-cases a currency code |

---

## Inheritance

`[meta].inherits` names a parent mapping id (its `source_model`, or
`doc_format:format_version`). At build time the inheritance chain is resolved
ancestor-first, so the child starts from the parent's full node set and only
declares its deltas:

- Re-declaring a node id **replaces the whole base node** — no field merge.
  Restate every field you want to keep. The node keeps the base's declaration
  position, so the element stays where the base emits it.
- Nodes the child adds are emitted after every base node, in the child's own
  declaration order (see [Declaration order is schema order](#declaration-order-is-schema-order)).
- The namespace entries of `[meta]` (`root_ns`, `namespaces`, `ns_defaults`)
  are inherited whole when the child omits them.
- `disabled = true` on a node removes it from the effective mapping.
- `disabled = true` in `[meta]` makes the mapping itself **inherit-only**: it
  can be inherited from but emits no spoke (see [cii.toml](cii.toml), which
  exists only to be specialized by [facturx.toml](facturx.toml)).

Missing parents and inheritance cycles fail the build.

[xrechnung.toml](xrechnung.toml) is the reference CIUS: its own `[meta]`
identity, a `detect` marker, and a single whole-node override making
`CustomizationID` required — everything else (~150 nodes) folds in from
[ubl.toml](ubl.toml) unchanged:

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

# Whole-node replacement: every field restated, not just `required`.
[Invoice.CustomizationID]
type = "identifier"
canonical_key = "SpecificationId"
required = true
normalize = ["trim"]
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

# A required collection of invoice lines.
[InvoiceLine]
type = "collection"
canonical_key = "InvoiceLines"
required = true
min_items = 1

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

Once it builds, the CLI offers two static authoring aids (no input document
needed):

```bash
krab-cli --keys <your-format>    # covered vs. unused canonical keys
krab-cli --analyze <your-format> # which conversions lose data, and what
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
| `E012` | Two canonical keys collide in generated code (same Rust field name, or one collection key in two scopes) |
| `E013` | Same canonical key mapped by two primary nodes in one spoke (use `fallbacks` or `clone_of`) |
| `E020` | `[meta].source_model` disagrees with the synthesized model id |
| `E021` | Node id does not resolve to a source path |
| `E022` | Collection node whose path is not a repeated field |
| `E023` | Scalar node whose path resolves to a struct, not a leaf |
| `E030` | Fallback target does not exist or is disabled |
| `E031` | Fallback target type incompatible with the primary |
| `E032` | Fallback target in a different scope |
| `E033` | Fallback reference cycle |
| `E040` | `multiple = "join"` without `join_with`, or `join_with` without join |
| `E041` | `min_items` on a non-collection node |
| `E043` | `multiple` combined with `fallbacks` |
| `E050` | Unknown adapter name |
| `E060` | `constant` on a collection node |
| `E061` | `constant` literal does not parse under the node's `type` |
| `E062` | `constant` combined with `fallbacks`, `multiple`, `adapter`, `normalize`, or `codec` |
| `E070` | `clone_of` on a collection, or combined with `canonical_key`, `constant`, `fallbacks`, `multiple`, or `adapter` |
| `E071` | `clone_of` target key not declared by a primary node in the same scope |
| `E072` | `clone_of` node's `type` differs from its target's |
| `E080` | Namespace prefix used by `root_ns`, `ns_defaults`, or a node's `ns` but not declared in `[meta.namespaces]` |
| `E081` | `ns` on an attribute or `$text` leaf (never prefixed) |
| `E083` | Structural node that names no element (nothing beneath it) or names the root (use `root_ns`) |
| `E084` | Unknown codec id |
| `E085` | `codec` on a collection, or codec `for_type` differs from the node's `type` |
| `E087` | Codec wire attribute collides with an attribute node on the same element |
| `W050` | `adapter` is deprecated (warning): use `normalize` or a codec |

Runtime (per-document) diagnostics — missing required values, type validation
failures, taken fallbacks, `CLONE_MISMATCH`, `CODEC_INVALID`,
`CODEC_WIRE_MISMATCH` — are reported with severity and a
source-node reference when a document is transformed; they never silently
vanish.
