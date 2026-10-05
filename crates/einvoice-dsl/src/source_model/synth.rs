//! Source-model synthesis: build the struct tree and per-node source paths from
//! the mapping nodes.
//!
//! A node's id mirrors the XML element tree, so both the typed source structs and
//! every node's `source_path` are derived here from the node ids, their
//! `type`/`required`/`xml`, and `[meta].root` — no separate `[source]` tree.
//! Codegen then emits the source structs from the same [`SourceModelMeta`].
//!
//! # Physical-element aliasing
//!
//! A collection or structural node may bind a physical element other than its
//! own id segment (`xml = "AdditionalReferencedDocument"`) and/or restrict
//! itself to the occurrences a `match` selector picks. Every such *logical*
//! node, and every other node bound to the same element, is an **alias** of
//! that element: the element is synthesized once as a repeated *physical* field
//! (`all_additional_referenced_document: Vec<Item>`, the item struct being the
//! union of every logical node's children) plus one *logical* field per logical
//! node (`tender_or_lot_referenced_document: Option<Box<Item>>` for a
//! structural node, `Vec<Item>` for a collection) carrying an [`AliasBinding`].
//! Logical fields are what the nodes' `source_path`s run through; the generated
//! reader fills them from the physical field by selector and the writer drains
//! them back, writing the selector values as discriminators.

use std::collections::BTreeMap;

use crate::codec::CodecTable;
use crate::error::{Diagnostic, Severity};
use crate::node::{NodeId, RawNode, Scope};
use crate::types::MappingType;

use super::meta::{AliasBinding, FieldMeta, FieldType, NamespaceMeta, SourceModelMeta, StructMeta};

/// The namespace configuration synthesis works from: the effective `[meta]`
/// entries `root_ns`, `[meta.ns_defaults]` and `[meta.namespaces]` (after
/// inheritance). The default — everything unprefixed, nothing declared — is
/// what a mapping without namespace meta gets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamespaceConfig {
    /// Prefix of the root element (`""` = unprefixed).
    pub root_prefix: String,
    /// Default prefix for scalar / valued elements.
    pub leaf_prefix: String,
    /// Default prefix for inferred interior and collection elements.
    pub aggregate_prefix: String,
    /// Declared namespaces, prefix → URI (`""` = default namespace).
    pub declared: BTreeMap<String, String>,
}

impl NamespaceConfig {
    /// Whether `prefix` must be, but is not, declared. The empty prefix needs
    /// no declaration (an undeclared default namespace just means "no
    /// namespace").
    fn undeclared(&self, prefix: &str) -> bool {
        !prefix.is_empty() && !self.declared.contains_key(prefix)
    }
}

/// Synthesizes the typed source model from the mapping nodes.
///
/// A node's id mirrors the XML element tree (its segments are XML element local
/// names), so the struct tree, the `Option`/`Vec` wrappers, the serde XML
/// renames, and each node's `source_path` can all be derived without a separate
/// `[source]` tree. Returns the model, a `NodeId → source_path` map (the dotted
/// snake_case Rust field path relative to the node's scope struct), and any
/// diagnostics.
///
/// Rules:
/// - The node's scope is its nearest enclosing `collection` ancestor, else root
///   (matching [`crate::resolve`]). Each scope has a root struct: the root scope's
///   is `root`; a collection scope's is the collection's item struct.
/// - A node's element path within its scope is its id minus the scope prefix.
/// - A leaf with descendant nodes is a *valued container* (a struct with a
///   `$text` `value` field plus its descendants' fields); a leaf without
///   descendants is a scalar field; a `collection` node is a `Vec<Item>` field.
/// - Every interior element, valued container and collection item gets its own
///   struct, named from its *physical* element path (`InvoiceLineItem`), so
///   same-named elements under different parents never share a struct or a
///   field order, while logical nodes aliasing one element do share its struct.
/// - Every scalar leaf is `Option<String>` (or `Vec<String>` with `multiple`);
///   `required` is enforced by the generated reader/writer as a
///   `REQUIRED_MISSING` diagnostic, never by failing deserialization.
/// - Every field carries an emission `order`: the declaration position of the
///   first node that contributes to it, so an inferred interior element sits
///   where its first descendant was declared. Codegen emits fields in that
///   order (attributes first), which is how the mapping's declaration order
///   becomes the emitted XML's element order.
/// - Every element field carries the namespace `prefix` it is written with:
///   the node's own `ns`, else the `leaf` default for scalar / valued elements
///   and the `aggregate` default for interior and collection elements. An
///   interior element named by a *structural node* (`ns` only, no `type`)
///   takes that node's `ns`. Attributes and `$text` are never prefixed.
/// - A node whose `codec` carries *wire attributes* (`wire = { "@format" =
///   "102" }`) is a valued container even without descendants: its struct gets
///   one attribute field per wire attribute, which the writer sets alongside
///   the encoded value.
/// - An interior element named by a structural node with `required = true` is
///   flagged `always_present`: the writer materializes it even when empty.
/// - A collection or structural node with a `match` selector, or sharing its
///   physical element with another such node, becomes a *logical* field next
///   to the element's one repeated *physical* field (see the module docs).
///
/// Namespace diagnostics (all errors): `E080` a prefix used by `root_ns`, the
/// defaults, or a node's `ns` is not declared in `[meta.namespaces]`; `E081`
/// `ns` on an attribute or `$text` leaf; `E083` a structural node that names no
/// element (no typed node beneath it, an attribute or `$text` binding, or the
/// root, whose prefix is `root_ns`). Codec diagnostic: `E087` a wire attribute
/// that collides with an attribute node the mapping declares on the same
/// element. (Unknown codec ids and type mismatches are the validator's
/// E084/E085.) Aliasing diagnostics: `E090` two logical nodes of one element
/// whose selectors can both match an item (or two without a selector); `E091`
/// `match` on a scalar node; `E092` a selector key that does not name a single
/// scalar declared beneath the element's logical nodes.
///
/// This entry point uses the default [`NamespaceConfig`] (nothing prefixed or
/// declared) and no codecs; [`synthesize_source_model_with`] takes the
/// mapping's.
pub fn synthesize_source_model(
    active: &BTreeMap<NodeId, RawNode>,
    root: &str,
    model_id: &str,
) -> (SourceModelMeta, BTreeMap<NodeId, String>, Vec<Diagnostic>) {
    synthesize_source_model_with(
        active,
        root,
        model_id,
        &NamespaceConfig::default(),
        &CodecTable::new(),
    )
}

/// [`synthesize_source_model`] with an explicit namespace configuration and
/// the shared codec table.
pub fn synthesize_source_model_with(
    active: &BTreeMap<NodeId, RawNode>,
    root: &str,
    model_id: &str,
    ns: &NamespaceConfig,
    codecs: &CodecTable,
) -> (SourceModelMeta, BTreeMap<NodeId, String>, Vec<Diagnostic>) {
    let mut structs: BTreeMap<String, StructMeta> = BTreeMap::new();
    structs.insert(root.to_string(), StructMeta::default());
    let mut source_paths: BTreeMap<NodeId, String> = BTreeMap::new();
    let mut diags: Vec<Diagnostic> = Vec::new();

    check_namespace_meta(ns, &mut diags);

    let ctx = Ctx {
        active,
        ns,
        codecs,
        root,
        aliases: Aliases::collect(active, root),
    };

    // Collection nodes define scopes (and item structs). Build the lookup first
    // so every node's scope and base struct can be computed.
    let collections: BTreeMap<NodeId, MappingType> = active
        .iter()
        .filter_map(|(id, n)| {
            n.ty.filter(|t| *t == MappingType::Collection)
                .map(|t| (id.clone(), t))
        })
        .collect();

    for (id, node) in active {
        if let Some(prefix) = node.ns.as_deref()
            && ns.undeclared(prefix)
        {
            diags.push(diag(
                "E080",
                Some(id),
                format!("node `{id}` uses namespace prefix `{prefix}`, which `[meta.namespaces]` does not declare"),
            ));
        }
        let Some(ty) = node.ty else {
            // Untyped tables are containers inferred from descendant ids, not
            // standalone nodes; nothing to place. A structural node is consumed
            // when the interior field it names is created — unless nothing is
            // beneath it, which is an authoring error. (A disabled-only
            // override has already been removed before synthesis.)
            if node.is_structural() {
                if id.as_str() == root {
                    diags.push(diag(
                        "E083",
                        Some(id),
                        format!("structural node `{id}` names the root element; set `[meta].root_ns` instead"),
                    ));
                } else if node.xml.as_deref().is_some_and(is_leaf_binding) {
                    diags.push(diag(
                        "E083",
                        Some(id),
                        format!("structural node `{id}` must bind an element: `xml` names an attribute or `$text`"),
                    ));
                } else if !has_descendant(active, id) {
                    diags.push(diag(
                        "E083",
                        Some(id),
                        format!("structural node `{id}` names no element: no typed node is declared beneath it"),
                    ));
                }
            }
            continue;
        };
        if node.match_.is_some() && ty != MappingType::Collection {
            diags.push(diag(
                "E091",
                Some(id),
                format!("`match` on scalar node `{id}`: a selector belongs on a collection or structural node"),
            ));
        }
        let scope = id.nearest_collection_scope(|a| collections.contains_key(a));
        let base = ctx.scope_struct(&scope);
        let segments = element_path(id, &scope, root);
        if segments.is_empty() {
            diags.push(synth_err(
                id,
                format!("node `{id}` has no element path under its scope"),
            ));
            continue;
        }

        match insert_node(&mut structs, &base, &segments, node, ty, id, &ctx) {
            Ok(path) => {
                source_paths.insert(id.clone(), path);
            }
            Err((code, msg)) => diags.push(diag(code, Some(id), msg)),
        }
    }

    check_selectors(&mut structs, &ctx, &mut diags);

    (
        SourceModelMeta {
            model_id: model_id.to_string(),
            root: root.to_string(),
            structs,
            namespaces: NamespaceMeta {
                root_prefix: ns.root_prefix.clone(),
                declared: ns.declared.clone(),
            },
        },
        source_paths,
        diags,
    )
}

/// Whether an `xml` binding names an attribute or the element text rather than
/// an element.
fn is_leaf_binding(xml: &str) -> bool {
    xml.starts_with('@') || xml == "$text"
}

/// `E080` for the `[meta]`-level prefixes (`root_ns`, `ns_defaults`) that are
/// not declared.
fn check_namespace_meta(ns: &NamespaceConfig, diags: &mut Vec<Diagnostic>) {
    for (what, prefix) in [
        ("root_ns", ns.root_prefix.as_str()),
        ("ns_defaults.leaf", ns.leaf_prefix.as_str()),
        ("ns_defaults.aggregate", ns.aggregate_prefix.as_str()),
    ] {
        if ns.undeclared(prefix) {
            diags.push(diag(
                "E080",
                None,
                format!("`[meta].{what}` uses namespace prefix `{prefix}`, which `[meta.namespaces]` does not declare"),
            ));
        }
    }
}

/// The invariant inputs of one synthesis run.
struct Ctx<'a> {
    active: &'a BTreeMap<NodeId, RawNode>,
    ns: &'a NamespaceConfig,
    codecs: &'a CodecTable,
    root: &'a str,
    aliases: Aliases,
}

impl Ctx<'_> {
    /// The struct a scope's nodes are placed into: the model root for root
    /// scope, or the collection's item struct for a collection scope.
    fn scope_struct(&self, scope: &Scope) -> String {
        match scope {
            Scope::Root => self.root.to_string(),
            Scope::Collection(coll) => self.element_struct(coll),
        }
    }

    /// The struct of the element a node id names, by its physical path.
    fn element_struct(&self, id: &NodeId) -> String {
        struct_name_for(&self.aliases.physical_id(id), self.root)
    }
}

/// One logical node's binding to its physical element.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Alias {
    /// Id of the physical element (the parent id plus the element name).
    physical: NodeId,
    /// XML local name of the physical element.
    xml: String,
    /// The raw selector (child paths → text), `None` for the default bucket.
    selector: Option<BTreeMap<String, String>>,
    /// Declaration position of the logical node (orders siblings for E090).
    position: usize,
}

impl Alias {
    /// The Rust name of the physical field holding every occurrence.
    fn physical_field(&self) -> String {
        format!("all_{}", snake_case(&self.xml))
    }

    /// The binding stored on the logical field; its selector is in raw key form
    /// until [`check_selectors`] resolves it to item field paths.
    fn binding(&self, node: &NodeId) -> AliasBinding {
        AliasBinding {
            physical: self.physical_field(),
            node: node.to_string(),
            selector: self
                .selector
                .iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

/// The aliasing structure of a mapping: which element-level nodes rename their
/// element, and which are logical nodes of a shared physical element.
#[derive(Debug, Default)]
struct Aliases {
    /// Collection / structural nodes with an `xml` element name: id → name.
    renames: BTreeMap<NodeId, String>,
    /// Logical nodes (an element bound by several nodes, or by a selector).
    logical: BTreeMap<NodeId, Alias>,
    /// The logical nodes of each physical element id, in declaration order.
    by_physical: BTreeMap<NodeId, Vec<NodeId>>,
}

impl Aliases {
    /// Groups every collection and structural node by the physical element it
    /// binds; a group with more than one node, or with any selector, is aliased.
    fn collect(active: &BTreeMap<NodeId, RawNode>, root: &str) -> Self {
        let mut renames = BTreeMap::new();
        let mut groups: BTreeMap<NodeId, Vec<(&NodeId, &RawNode)>> = BTreeMap::new();
        for (id, node) in active {
            let element_level = node.ty == Some(MappingType::Collection) || node.is_structural();
            if !element_level || id.as_str() == root {
                continue;
            }
            let own = id.segments().last().unwrap_or_default().to_string();
            let xml = match node.xml.as_deref() {
                Some(xml) if !is_leaf_binding(xml) => xml.to_string(),
                _ => own.clone(),
            };
            if xml != own {
                renames.insert(id.clone(), xml.clone());
            }
            let physical = match id.parent() {
                Some(parent) => NodeId::new(format!("{parent}.{xml}")),
                None => NodeId::new(xml),
            };
            groups.entry(physical).or_default().push((id, node));
        }
        let mut logical = BTreeMap::new();
        let mut by_physical = BTreeMap::new();
        for (physical, mut members) in groups {
            if members.len() < 2 && members.iter().all(|(_, n)| n.match_.is_none()) {
                continue;
            }
            members.sort_by_key(|(_, n)| n.position);
            let xml = physical.segments().last().unwrap_or_default().to_string();
            let mut ids = Vec::new();
            for (id, node) in members {
                logical.insert(
                    id.clone(),
                    Alias {
                        physical: physical.clone(),
                        xml: xml.clone(),
                        selector: node.match_.clone(),
                        position: node.position,
                    },
                );
                ids.push(id.clone());
            }
            by_physical.insert(physical, ids);
        }
        Self {
            renames,
            logical,
            by_physical,
        }
    }

    /// The ids that name the same physical element path as `id` from the point
    /// of view of the logical nodes sharing its nearest aliased ancestor: `id`
    /// itself, plus `id` with that ancestor swapped for each of its logical
    /// siblings. Children of logical nodes of one element land in one union
    /// item struct, so whether an element has children is decided across all
    /// of them.
    fn equivalents(&self, id: &NodeId) -> Vec<NodeId> {
        let segs: Vec<&str> = id.segments().collect();
        let mut out = vec![id.clone()];
        for len in (1..segs.len()).rev() {
            let prefix = NodeId::new(segs[..len].join("."));
            let Some(alias) = self.logical.get(&prefix) else {
                continue;
            };
            let rest = segs[len..].join(".");
            for sibling in self.by_physical.get(&alias.physical).into_iter().flatten() {
                if *sibling != prefix {
                    out.push(NodeId::new(format!("{sibling}.{rest}")));
                }
            }
            break;
        }
        out
    }

    /// The id of the physical element path a node id denotes: every segment
    /// that is a renamed or aliased element-level node is replaced by the
    /// element's XML name. Struct names derive from this, so logical nodes of
    /// one element share its struct (and their descendants share its
    /// interior structs).
    fn physical_id(&self, id: &NodeId) -> NodeId {
        let mut prefix = String::new();
        let mut out: Vec<String> = Vec::new();
        for seg in id.segments() {
            if !prefix.is_empty() {
                prefix.push('.');
            }
            prefix.push_str(seg);
            let name = self
                .renames
                .get(&NodeId::new(prefix.as_str()))
                .cloned()
                .unwrap_or_else(|| seg.to_string());
            out.push(name);
        }
        NodeId::new(out.join("."))
    }
}

/// The prefix an element at `element_id` is written with when it appears as an
/// *interior* segment of some node's path: a structural or typed node's own
/// `ns`, else the leaf default for a valued element (a typed non-collection
/// node with descendants) and the aggregate default for everything else.
fn element_prefix(
    active: &BTreeMap<NodeId, RawNode>,
    element_id: &NodeId,
    ns: &NamespaceConfig,
) -> String {
    match active.get(element_id) {
        Some(node) if node.ns.is_some() => node.ns.clone().unwrap_or_default(),
        Some(node) if node.ty.is_some_and(|t| t != MappingType::Collection) => {
            ns.leaf_prefix.clone()
        }
        _ => ns.aggregate_prefix.clone(),
    }
}

/// The XML local name an interior element at `element_id` is written as: the
/// node's own element `xml` rename when it has one, else its id segment.
fn element_xml_name(active: &BTreeMap<NodeId, RawNode>, element_id: &NodeId, seg: &str) -> String {
    active
        .get(element_id)
        .and_then(|n| n.xml.as_deref())
        .filter(|xml| !is_leaf_binding(xml))
        .unwrap_or(seg)
        .to_string()
}

/// The struct name of the element at `id`: the CamelCase of its id segments
/// (the root segment dropped), joined. One struct per element *path* —
/// `InvoiceLine.Item` → `InvoiceLineItem`,
/// `Invoice.AccountingSupplierParty.Party` → `AccountingSupplierPartyParty` —
/// so two elements that share a local name (CII's header and line
/// `ApplicableTradeTax`) never share a struct, and each keeps its own field
/// order. Collection item structs are named the same way from the collection's
/// id. Callers pass the *physical* id ([`Aliases::physical_id`]).
fn struct_name_for(id: &NodeId, root: &str) -> String {
    let mut segs = id.segments().peekable();
    if segs.peek() == Some(&root) && id.segments().count() > 1 {
        segs.next();
    }
    segs.map(camel_case).collect()
}

/// A node's XML element path within its scope: its id segments with the scope
/// prefix removed. Root scope drops a leading `root` segment if present (so
/// `Invoice.ID` → `[ID]`, while a bare collection id like `InvoiceLine` keeps all
/// segments); a collection scope drops the collection node's id prefix.
fn element_path(id: &NodeId, scope: &Scope, root: &str) -> Vec<String> {
    let segs: Vec<String> = id.segments().map(str::to_string).collect();
    match scope {
        Scope::Root => {
            if segs.first().map(String::as_str) == Some(root) {
                segs[1..].to_vec()
            } else {
                segs
            }
        }
        Scope::Collection(coll) => {
            let skip = coll.segments().count();
            segs[skip..].to_vec()
        }
    }
}

/// Inserts one node's element path into the struct table, creating interior
/// structs as needed, and returns the node's `source_path` (dotted snake field
/// path relative to its scope struct).
///
/// `base` names the scope struct; `segments` is the element path within it;
/// `ctx` supplies the other nodes, the namespace defaults, the codecs and the
/// aliasing structure. Compatible existing fields keep the earliest
/// contributing node position.
///
/// # Errors
///
/// Returns `(code, message)`: `E024` for incompatible field bindings or
/// `multiple` on a collection, attribute, `$text` leaf, or valued container;
/// `E081` for `ns` on an attribute or `$text` leaf; `E087` for a codec wire
/// attribute colliding with a declared attribute node. Earlier changes to
/// `structs` are retained on error. The caller turns these into diagnostics
/// and continues processing other nodes.
///
/// # Panics
///
/// Panics if `segments` is empty.
fn insert_node(
    structs: &mut BTreeMap<String, StructMeta>,
    base: &str,
    segments: &[String],
    node: &RawNode,
    ty: MappingType,
    id: &NodeId,
    ctx: &Ctx,
) -> Result<String, (&'static str, String)> {
    let active = ctx.active;
    let ns = ctx.ns;
    let e024 = |msg: String| ("E024", msg);
    // The codec's wire attributes, if the node names a known codec that has
    // any: they make the element a valued container with attribute fields.
    let wire: Vec<&String> = node
        .codec
        .as_deref()
        .and_then(|c| ctx.codecs.get(c))
        .map(|c| c.wire.keys().collect())
        .unwrap_or_default();
    // The node's own prefix: its `ns`, else the default for its kind.
    let own_prefix = |default: &str| node.ns.clone().unwrap_or_else(|| default.to_string());
    // Full ids of the interior segments: `segments` is the id minus the scope
    // prefix, so segment `i` is the id truncated after `stripped + i + 1`
    // segments.
    let all: Vec<&str> = id.segments().collect();
    let stripped = all.len() - segments.len();

    // Descend/create interior structs for all but the final segment.
    let mut current = base.to_string();
    let mut path_parts: Vec<String> = Vec::new();
    for (i, seg) in segments[..segments.len() - 1].iter().enumerate() {
        let field = snake_case(seg);
        let interior_id = NodeId::new(all[..stripped + i + 1].join("."));
        let struct_name = ctx.element_struct(&interior_id);
        let always_present = active
            .get(&interior_id)
            .is_some_and(RawNode::is_always_present);
        let meta = match ctx.aliases.logical.get(&interior_id) {
            // A logical structural node: the element's physical field holds
            // every occurrence; this node's own field holds the one it selects.
            Some(alias) => {
                upsert_physical(
                    structs,
                    &current,
                    alias,
                    &interior_id,
                    &struct_name,
                    node,
                    ctx,
                )
                .map_err(e024)?;
                FieldMeta {
                    optional: false,
                    repeated: false,
                    ty: FieldType::Struct(struct_name.clone()),
                    xml: None,
                    prefix: String::new(),
                    always_present,
                    order: node.position,
                    alias: Some(alias.binding(&interior_id)),
                }
            }
            None => FieldMeta {
                optional: false,
                repeated: false,
                ty: FieldType::Struct(struct_name.clone()),
                xml: Some(element_xml_name(active, &interior_id, seg)),
                prefix: element_prefix(active, &interior_id, ns),
                always_present,
                order: node.position,
                alias: None,
            },
        };
        upsert_field(structs, &current, &field, meta).map_err(e024)?;
        structs.entry(struct_name.clone()).or_default();
        path_parts.push(field);
        current = struct_name;
    }

    let last = &segments[segments.len() - 1];
    // Leaves are always `Option<String>` (with a serde `default`), whatever
    // `required` says: a document missing the element must still *parse*, so
    // the generated reader/writer can report REQUIRED_MISSING as a structured
    // diagnostic instead of the parse failing with a raw serde error.
    let optional = true;

    // `multiple` opts a plain scalar element leaf into a `Vec<String>` source
    // field. Every other node shape either cannot repeat in XML (attributes,
    // element text) or already repeats structurally (collections).
    let multi = node.multiple.is_some();

    // Collection node: a repeated struct field; children populate the item struct.
    if ty == MappingType::Collection {
        if multi {
            return Err(e024(
                "`multiple` is not valid on a collection node (a collection already repeats)"
                    .to_string(),
            ));
        }
        let field = snake_case(last);
        let item = ctx.element_struct(id);
        let meta = match ctx.aliases.logical.get(id) {
            // A logical collection: its items are the physical element's
            // occurrences its selector picks.
            Some(alias) => {
                upsert_physical(structs, &current, alias, id, &item, node, ctx).map_err(e024)?;
                FieldMeta {
                    optional: false,
                    repeated: true,
                    ty: FieldType::Struct(item.clone()),
                    xml: None,
                    prefix: String::new(),
                    always_present: false,
                    order: node.position,
                    alias: Some(alias.binding(id)),
                }
            }
            None => FieldMeta {
                optional: false,
                repeated: true,
                ty: FieldType::Struct(item.clone()),
                xml: Some(node.xml.clone().unwrap_or_else(|| last.clone())),
                prefix: own_prefix(&ns.aggregate_prefix),
                always_present: false,
                order: node.position,
                alias: None,
            },
        };
        upsert_field(structs, &current, &field, meta).map_err(e024)?;
        structs.entry(item).or_default();
        path_parts.push(field);
        return Ok(path_parts.join("."));
    }

    // Attribute leaf (`xml = "@..."`): a scalar field on the current struct.
    if let Some(xml) = node.xml.as_deref()
        && xml.starts_with('@')
    {
        if multi {
            return Err(e024(
                "`multiple` is not valid on an attribute leaf (an XML attribute cannot repeat)"
                    .to_string(),
            ));
        }
        if node.ns.is_some() {
            return Err((
                "E081",
                "`ns` is not valid on an attribute leaf (attributes are never prefixed)"
                    .to_string(),
            ));
        }
        let field = snake_case(last);
        upsert_field(
            structs,
            &current,
            &field,
            FieldMeta {
                optional,
                repeated: false,
                ty: FieldType::Scalar,
                xml: Some(xml.to_string()),
                prefix: String::new(),
                always_present: false,
                order: node.position,
                alias: None,
            },
        )
        .map_err(e024)?;
        path_parts.push(field);
        return Ok(path_parts.join("."));
    }

    // Element text override (`xml = "$text"`): a value field on the current struct.
    if node.xml.as_deref() == Some("$text") {
        if multi {
            return Err(e024(
                "`multiple` is not valid on a `$text` leaf (element text cannot repeat)"
                    .to_string(),
            ));
        }
        if node.ns.is_some() {
            return Err((
                "E081",
                "`ns` is not valid on a `$text` leaf (set it on the element's own node)"
                    .to_string(),
            ));
        }
        upsert_field(
            structs,
            &current,
            "value",
            FieldMeta {
                optional,
                repeated: false,
                ty: FieldType::Scalar,
                xml: Some("$text".to_string()),
                prefix: String::new(),
                always_present: false,
                order: node.position,
                alias: None,
            },
        )
        .map_err(e024)?;
        path_parts.push("value".to_string());
        return Ok(path_parts.join("."));
    }

    // A typed element with descendant nodes — or with codec wire attributes — is
    // a *valued container*: its own value is the element text, carried by a
    // `$text` `value` field inside a struct that also holds its descendants
    // (e.g. an `@currencyID` attribute) and the wire attributes.
    if ctx
        .aliases
        .equivalents(id)
        .iter()
        .any(|eq| has_descendant(active, eq))
        || !wire.is_empty()
    {
        if multi {
            return Err(e024(
                "`multiple` is not valid on a valued container (model the repetition as a collection instead)"
                    .to_string(),
            ));
        }
        let field = snake_case(last);
        let struct_name = ctx.element_struct(id);
        let rename = node.xml.clone().unwrap_or_else(|| last.clone());
        upsert_field(
            structs,
            &current,
            &field,
            FieldMeta {
                optional: false,
                repeated: false,
                ty: FieldType::Struct(struct_name.clone()),
                xml: Some(rename),
                prefix: own_prefix(&ns.leaf_prefix),
                always_present: false,
                order: node.position,
                alias: None,
            },
        )
        .map_err(e024)?;
        structs.entry(struct_name.clone()).or_default();
        upsert_field(
            structs,
            &struct_name,
            "value",
            FieldMeta {
                optional,
                repeated: false,
                ty: FieldType::Scalar,
                xml: Some("$text".to_string()),
                prefix: String::new(),
                always_present: false,
                order: node.position,
                alias: None,
            },
        )
        .map_err(e024)?;
        for attr in wire {
            // A declared attribute node on the same element would compete with
            // the codec for the attribute's value.
            if active.contains_key(&NodeId::new(format!("{id}.{attr}"))) {
                return Err((
                    "E087",
                    format!(
                        "codec wire attribute `@{attr}` collides with the attribute node `{id}.{attr}`"
                    ),
                ));
            }
            upsert_field(
                structs,
                &struct_name,
                &snake_case(attr),
                FieldMeta {
                    optional: true,
                    repeated: false,
                    ty: FieldType::Scalar,
                    xml: Some(format!("@{attr}")),
                    prefix: String::new(),
                    always_present: false,
                    order: node.position,
                    alias: None,
                },
            )
            .map_err(e024)?;
        }
        path_parts.push(field);
        path_parts.push("value".to_string());
        return Ok(path_parts.join("."));
    }

    // Plain scalar leaf: a field on the current struct, optionally renamed.
    // With `multiple` declared the field is `Vec<String>` (repeated, never
    // `Option`-wrapped) so repeated source elements parse instead of failing.
    let field = snake_case(last);
    let rename = node.xml.clone().unwrap_or_else(|| last.clone());
    upsert_field(
        structs,
        &current,
        &field,
        FieldMeta {
            optional: optional && !multi,
            repeated: multi,
            ty: FieldType::Scalar,
            xml: Some(rename),
            prefix: own_prefix(&ns.leaf_prefix),
            always_present: false,
            order: node.position,
            alias: None,
        },
    )
    .map_err(e024)?;
    path_parts.push(field);
    Ok(path_parts.join("."))
}

/// Ensures the physical repeated field of an aliased element exists on struct
/// `parent`: `all_<element>: Vec<Item>`, written with the logical node's `ns`
/// or the aggregate default. Every logical node's descendants pass through
/// here, so the field takes the earliest contributing position.
fn upsert_physical(
    structs: &mut BTreeMap<String, StructMeta>,
    parent: &str,
    alias: &Alias,
    logical_id: &NodeId,
    item: &str,
    contributing: &RawNode,
    ctx: &Ctx,
) -> Result<(), String> {
    upsert_field(
        structs,
        parent,
        &alias.physical_field(),
        FieldMeta {
            optional: false,
            repeated: true,
            ty: FieldType::Struct(item.to_string()),
            xml: Some(alias.xml.clone()),
            prefix: element_prefix(ctx.active, logical_id, ctx.ns),
            always_present: false,
            order: contributing.position,
            alias: None,
        },
    )
    .map_err(|msg| {
        format!(
            "{msg}: the logical nodes of `{}` must agree on `ns`",
            alias.xml
        )
    })?;
    structs.entry(item.to_string()).or_default();
    Ok(())
}

/// Resolves every logical field's selector against its item struct (`E092`
/// when a key names nothing, something repeated, or an element with children
/// but no text), rewriting the keys to item field paths for codegen, and
/// checks that the logical nodes of each element are disjoint (`E090`).
fn check_selectors(
    structs: &mut BTreeMap<String, StructMeta>,
    ctx: &Ctx,
    diags: &mut Vec<Diagnostic>,
) {
    // E090: pairwise over the logical nodes of each physical element.
    for ids in ctx.aliases.by_physical.values() {
        for (i, later) in ids.iter().enumerate() {
            let later_alias = &ctx.aliases.logical[later];
            for earlier in &ids[..i] {
                let earlier_alias = &ctx.aliases.logical[earlier];
                if selectors_overlap(earlier_alias, later_alias) {
                    let what = match (&earlier_alias.selector, &later_alias.selector) {
                        (None, None) => "neither has a `match` selector, so both would take every unmatched item".to_string(),
                        _ => "their `match` selectors share no key with different values, so one item could satisfy both".to_string(),
                    };
                    diags.push(diag(
                        "E090",
                        Some(later),
                        format!(
                            "logical nodes `{earlier}` and `{later}` both bind `{}` but overlap: {what}",
                            later_alias.xml
                        ),
                    ));
                }
            }
        }
    }

    // E092: resolve each selector key to a scalar field path in the item.
    let logical_fields: Vec<(String, String, String, AliasBinding)> = structs
        .iter()
        .flat_map(|(st, meta)| {
            meta.fields.iter().filter_map(move |(name, f)| {
                let alias = f.alias.clone()?;
                let FieldType::Struct(item) = &f.ty else {
                    return None;
                };
                Some((st.clone(), name.clone(), item.clone(), alias))
            })
        })
        .collect();
    for (st, name, item, alias) in logical_fields {
        let node = NodeId::new(alias.node.as_str());
        let raw = ctx
            .aliases
            .logical
            .get(&node)
            .and_then(|a| a.selector.as_ref());
        let Some(raw) = raw else {
            continue;
        };
        if raw.is_empty() {
            diags.push(diag(
                "E092",
                Some(&node),
                format!("`match` on `{node}` is empty; name at least one child value to select by"),
            ));
            continue;
        }
        let mut resolved = Vec::new();
        for (key, value) in raw {
            match resolve_selector_key(structs, &item, key) {
                Ok(path) => resolved.push((path, value.clone())),
                Err(reason) => diags.push(diag(
                    "E092",
                    Some(&node),
                    format!("`match` key `{key}` on `{node}` does not resolve: {reason}"),
                )),
            }
        }
        if let Some(field) = structs.get_mut(&st).and_then(|m| m.fields.get_mut(&name))
            && let Some(binding) = field.alias.as_mut()
        {
            binding.selector = resolved;
        }
    }
}

/// Whether two logical nodes of one element could claim the same item: both
/// without a selector, or neither selector naming a key the other gives a
/// different value.
fn selectors_overlap(a: &Alias, b: &Alias) -> bool {
    match (&a.selector, &b.selector) {
        (None, None) => true,
        (Some(a), Some(b)) => !a
            .iter()
            .any(|(key, value)| b.get(key).is_some_and(|other| other != value)),
        _ => false,
    }
}

/// Resolves a selector key (`TypeCode`, `TaxScheme.ID`, `ID.@schemeID`,
/// `@schemeID`, `$text`) against the item struct to the dotted field path of
/// the scalar it names. Segments are matched by XML binding, so attributes and
/// text need no snake-casing by the author; a valued container resolves to its
/// `value`.
fn resolve_selector_key(
    structs: &BTreeMap<String, StructMeta>,
    item: &str,
    key: &str,
) -> Result<String, String> {
    if key.is_empty() {
        return Err("the key is empty".to_string());
    }
    let mut current = item.to_string();
    let mut path: Vec<String> = Vec::new();
    let segments: Vec<&str> = key.split('.').collect();
    for (i, seg) in segments.iter().enumerate() {
        let is_last = i + 1 == segments.len();
        let meta = structs
            .get(&current)
            .ok_or_else(|| format!("struct `{current}` is missing"))?;
        let Some((name, field)) = meta
            .fields
            .iter()
            .find(|(_, f)| f.xml.as_deref() == Some(*seg))
        else {
            return Err(format!(
                "`{seg}` is not declared beneath the element by any of its logical nodes"
            ));
        };
        if field.repeated {
            return Err(format!(
                "`{seg}` repeats; a selector compares a single value"
            ));
        }
        path.push(name.clone());
        match &field.ty {
            FieldType::Scalar if is_last => return Ok(path.join(".")),
            FieldType::Scalar => {
                return Err(format!(
                    "`{seg}` is a value; it has no child `{}`",
                    segments[i + 1]
                ));
            }
            FieldType::Struct(inner) if is_last => {
                let has_text = structs
                    .get(inner)
                    .and_then(|m| m.fields.get("value"))
                    .is_some_and(FieldMeta::is_text);
                if has_text {
                    path.push("value".to_string());
                    return Ok(path.join("."));
                }
                return Err(format!(
                    "`{seg}` is an element with children but no text of its own; name one of its children"
                ));
            }
            FieldType::Struct(inner) => current = inner.clone(),
        }
    }
    unreachable!("a non-empty key returns inside the loop")
}

/// Whether any active node has `id` as a strict id prefix (i.e. `id` is a parent
/// element of another node).
///
/// `active` is sorted by id, and every descendant shares the `"{id}."` prefix, so
/// the descendants form one contiguous range. The first key at or after that
/// prefix is a descendant iff any exists — an `O(log n)` range probe rather than
/// an `O(n)` scan per node.
fn has_descendant(active: &BTreeMap<NodeId, RawNode>, id: &NodeId) -> bool {
    let prefix = format!("{id}.");
    active
        .range(NodeId::new(prefix.clone())..)
        .next()
        .is_some_and(|(other, _)| other.as_str().starts_with(&prefix))
}

/// Inserts `field` into struct `struct_name`, creating the struct if absent.
/// Re-inserting an identically bound field is fine (two nodes contributing to
/// the same struct): it keeps the earliest emission `order` and *or*s the
/// `always_present` flag; a conflicting redefinition is an error (E024).
fn upsert_field(
    structs: &mut BTreeMap<String, StructMeta>,
    struct_name: &str,
    field: &str,
    meta: FieldMeta,
) -> Result<(), String> {
    let entry = structs.entry(struct_name.to_string()).or_default();
    match entry.fields.get_mut(field) {
        Some(existing) if !existing.same_binding(&meta) => Err(format!(
            "synthesized field `{struct_name}.{field}` is defined two incompatible ways"
        )),
        Some(existing) => {
            existing.order = existing.order.min(meta.order);
            existing.always_present |= meta.always_present;
            Ok(())
        }
        None => {
            entry.fields.insert(field.to_string(), meta);
            Ok(())
        }
    }
}

/// An `E024` synthesis diagnostic for `id`.
fn synth_err(id: &NodeId, message: String) -> Diagnostic {
    diag("E024", Some(id), message)
}

/// An error-severity synthesis diagnostic.
fn diag(code: &str, id: Option<&NodeId>, message: String) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity: Severity::Error,
        source_node: id.map(ToString::to_string),
        message,
        span: None,
    }
}

/// Converts a `snake_case`/`mixed` name to `CamelCase` (e.g. `legal_monetary_total`
/// → `LegalMonetaryTotal`, `PayableAmount` → `PayableAmount`).
fn camel_case(s: &str) -> String {
    s.split('_')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Converts an XML element/attribute local name to a `snake_case` Rust field name
/// (e.g. `IssueDate` → `issue_date`, `currencyID` → `currency_id`).
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            if i != 0 && (prev_lower || next_lower) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::resolve::resolve_path;
    use super::FieldType::{Scalar, Struct};
    use super::*;

    #[test]
    fn test_snake_case_xml_names() {
        assert_eq!(snake_case("ID"), "id");
        assert_eq!(snake_case("IssueDate"), "issue_date");
        assert_eq!(snake_case("currencyID"), "currency_id");
        assert_eq!(snake_case("UUID"), "uuid");
        assert_eq!(snake_case("InvoicedQuantity"), "invoiced_quantity");
    }

    fn raw(toml_src: &str) -> RawNode {
        toml::from_str(toml_src).expect("raw node parses")
    }

    /// Builds raw nodes from `(id, body)` pairs, numbering them in slice order
    /// exactly as the parser numbers tables in document order.
    fn nodes(pairs: &[(&str, &str)]) -> BTreeMap<NodeId, RawNode> {
        pairs
            .iter()
            .enumerate()
            .map(|(position, (id, body))| {
                let mut node = raw(body);
                node.position = position;
                (NodeId::new(*id), node)
            })
            .collect()
    }

    /// The emission-ordered field names of `structs[name]`.
    fn ordered(model: &SourceModelMeta, name: &str) -> Vec<String> {
        model.structs[name]
            .ordered_fields()
            .into_iter()
            .map(|(field, _)| field.clone())
            .collect()
    }

    fn synth(pairs: &[(&str, &str)]) -> (SourceModelMeta, BTreeMap<NodeId, String>) {
        let (model, paths, diags) = synthesize_source_model(&nodes(pairs), "Invoice", "ubl:2.1");
        assert!(diags.is_empty(), "unexpected synth diagnostics: {diags:?}");
        (model, paths)
    }

    #[test]
    fn test_synth_scalar_leaf_is_option_even_when_required() {
        // `required` is a reader/writer diagnostic, not a parse constraint: a
        // document missing the element must still deserialize.
        let (model, paths) = synth(&[
            (
                "Invoice.ID",
                r#"type = "identifier"
            required = true"#,
            ),
            ("Invoice.IssueDate", r#"type = "date""#),
        ]);
        let inv = &model.structs["Invoice"];
        assert_eq!(inv.fields["id"].ty, Scalar);
        assert!(inv.fields["id"].optional, "required leaf is still Option");
        assert_eq!(inv.fields["id"].xml.as_deref(), Some("ID"));
        assert!(inv.fields["issue_date"].optional);
        assert_eq!(paths[&NodeId::new("Invoice.ID")], "id");
        assert_eq!(paths[&NodeId::new("Invoice.IssueDate")], "issue_date");
    }

    #[test]
    fn test_synth_valued_container_with_attribute() {
        // PayableAmount carries a decimal text value AND a currencyID attribute.
        let (model, paths) = synth(&[
            (
                "Invoice.LegalMonetaryTotal.PayableAmount",
                r#"type = "decimal"
                required = true"#,
            ),
            (
                "Invoice.LegalMonetaryTotal.PayableAmount.currencyID",
                r#"xml = "@currencyID"
                type = "currency""#,
            ),
        ]);
        // Invoice.legal_monetary_total: LegalMonetaryTotal struct.
        assert_eq!(
            model.structs["Invoice"].fields["legal_monetary_total"].ty,
            Struct("LegalMonetaryTotal".into())
        );
        // LegalMonetaryTotal.payable_amount: PayableAmount struct.
        assert_eq!(
            model.structs["LegalMonetaryTotal"].fields["payable_amount"].ty,
            Struct("LegalMonetaryTotalPayableAmount".into())
        );
        // PayableAmount has a $text value field and the currencyID attribute.
        let pa = &model.structs["LegalMonetaryTotalPayableAmount"];
        assert_eq!(pa.fields["value"].xml.as_deref(), Some("$text"));
        assert!(pa.fields["value"].optional, "value leaf is always Option");
        assert_eq!(pa.fields["currency_id"].xml.as_deref(), Some("@currencyID"));
        assert!(pa.fields["currency_id"].optional);
        assert_eq!(
            paths[&NodeId::new("Invoice.LegalMonetaryTotal.PayableAmount")],
            "legal_monetary_total.payable_amount.value"
        );
        assert_eq!(
            paths[&NodeId::new("Invoice.LegalMonetaryTotal.PayableAmount.currencyID")],
            "legal_monetary_total.payable_amount.currency_id"
        );
    }

    #[test]
    fn test_synth_collection_and_item_children() {
        let (model, paths) = synth(&[
            (
                "InvoiceLine",
                r#"type = "collection"
                canonical_key = "InvoiceLines""#,
            ),
            ("InvoiceLine.ID", r#"type = "identifier""#),
            ("InvoiceLine.Item.Name", r#"type = "string""#),
        ]);
        // Root carries a Vec<InvoiceLine>.
        let coll = &model.structs["Invoice"].fields["invoice_line"];
        assert!(coll.repeated);
        assert_eq!(coll.ty, Struct("InvoiceLine".into()));
        assert_eq!(coll.xml.as_deref(), Some("InvoiceLine"));
        // Item children resolve against the item struct.
        assert_eq!(model.structs["InvoiceLine"].fields["id"].ty, Scalar);
        assert_eq!(
            model.structs["InvoiceLine"].fields["item"].ty,
            Struct("InvoiceLineItem".into())
        );
        assert_eq!(model.structs["InvoiceLineItem"].fields["name"].ty, Scalar);
        assert_eq!(paths[&NodeId::new("InvoiceLine")], "invoice_line");
        assert_eq!(paths[&NodeId::new("InvoiceLine.ID")], "id");
        assert_eq!(paths[&NodeId::new("InvoiceLine.Item.Name")], "item.name");
    }

    #[test]
    fn test_synth_resolves_against_itself() {
        // The synthesized model is consistent: every node's source_path resolves.
        let (model, paths) = synth(&[
            ("Invoice.ID", r#"type = "identifier""#),
            (
                "Invoice.LegalMonetaryTotal.PayableAmount",
                r#"type = "decimal""#,
            ),
        ]);
        for path in paths.values() {
            assert!(
                resolve_path(&model, path).is_ok(),
                "synthesized path `{path}` must resolve"
            );
        }
    }

    #[test]
    fn test_synth_is_deterministic() {
        let pairs: &[(&str, &str)] = &[
            ("Invoice.ID", r#"type = "identifier""#),
            ("Invoice.IssueDate", r#"type = "date""#),
            ("InvoiceLine", r#"type = "collection""#),
            ("InvoiceLine.ID", r#"type = "identifier""#),
        ];
        let a = synthesize_source_model(&nodes(pairs), "Invoice", "ubl:2.1");
        let b = synthesize_source_model(&nodes(pairs), "Invoice", "ubl:2.1");
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn test_synth_multiple_leaf_is_repeated_vec() {
        let (model, paths) = synth(&[(
            "Invoice.Note",
            r#"type = "string"
            multiple = "join"
            join_with = "\n""#,
        )]);
        let note = &model.structs["Invoice"].fields["note"];
        assert!(note.repeated, "multiple leaf must synthesize Vec<String>");
        assert!(!note.optional, "a Vec is never Option-wrapped");
        assert_eq!(note.ty, Scalar);
        assert_eq!(paths[&NodeId::new("Invoice.Note")], "note");
    }

    #[test]
    fn test_synth_multiple_on_attribute_is_error() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[(
                "Invoice.Amount.currencyID",
                r#"xml = "@currencyID"
                type = "currency"
                multiple = "first""#,
            )]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E024" && d.message.contains("attribute")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_multiple_on_collection_is_error() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[(
                "InvoiceLine",
                r#"type = "collection"
                multiple = "first""#,
            )]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E024" && d.message.contains("collection")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_multiple_on_valued_container_is_error() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[
                (
                    "Invoice.Amount",
                    r#"type = "decimal"
                    multiple = "first""#,
                ),
                (
                    "Invoice.Amount.currencyID",
                    r#"xml = "@currencyID"
                    type = "currency""#,
                ),
            ]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E024" && d.message.contains("valued container")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_field_order_follows_declaration_not_name() {
        // Declared Zeta, Alpha: the struct emits them in that order even though
        // the field map (and the id map) sort Alpha first.
        let (model, _) = synth(&[
            ("Invoice.Zeta", r#"type = "string""#),
            ("Invoice.Alpha", r#"type = "string""#),
        ]);
        assert_eq!(ordered(&model, "Invoice"), ["zeta", "alpha"]);
        assert_eq!(model.structs["Invoice"].fields["zeta"].order, 0);
        assert_eq!(model.structs["Invoice"].fields["alpha"].order, 1);
    }

    #[test]
    fn test_synth_interior_takes_first_descendants_position() {
        // `Totals` is never declared itself; it sits where its first declared
        // leaf (`Totals.Net`, position 0) is — before `A` (1) — and a later leaf
        // under it (`Totals.Gross`, 2) does not move it.
        let (model, _) = synth(&[
            ("Invoice.Totals.Net", r#"type = "decimal""#),
            ("Invoice.A", r#"type = "string""#),
            ("Invoice.Totals.Gross", r#"type = "decimal""#),
        ]);
        assert_eq!(ordered(&model, "Invoice"), ["totals", "a"]);
        assert_eq!(model.structs["Invoice"].fields["totals"].order, 0);
        assert_eq!(ordered(&model, "Totals"), ["net", "gross"]);
    }

    #[test]
    fn test_synth_collection_and_scalars_interleave_by_declaration() {
        let (model, _) = synth(&[
            ("Invoice.ID", r#"type = "identifier""#),
            (
                "Invoice.AllowanceCharge",
                r#"type = "collection"
                canonical_key = "Charges""#,
            ),
            ("Invoice.AllowanceCharge.Amount", r#"type = "decimal""#),
            ("Invoice.TaxTotal.TaxAmount", r#"type = "decimal""#),
            (
                "InvoiceLine",
                r#"type = "collection"
                canonical_key = "Lines""#,
            ),
            ("InvoiceLine.ID", r#"type = "identifier""#),
        ]);
        assert_eq!(
            ordered(&model, "Invoice"),
            ["id", "allowance_charge", "tax_total", "invoice_line"]
        );
    }

    #[test]
    fn test_synth_valued_container_emits_attribute_before_text() {
        // The attribute is declared after the element text, but attributes
        // always come first in emission order.
        let (model, _) = synth(&[
            ("Invoice.Amount", r#"type = "decimal""#),
            (
                "Invoice.Amount.currencyID",
                r#"xml = "@currencyID"
                type = "currency""#,
            ),
        ]);
        assert_eq!(ordered(&model, "Amount"), ["currency_id", "value"]);
    }

    /// A UBL-like namespace configuration: default-namespace root, `cbc`
    /// leaves, `cac` aggregates.
    fn ubl_ns() -> NamespaceConfig {
        NamespaceConfig {
            root_prefix: String::new(),
            leaf_prefix: "cbc".into(),
            aggregate_prefix: "cac".into(),
            declared: [
                ("".to_string(), "urn:invoice".to_string()),
                ("cbc".to_string(), "urn:cbc".to_string()),
                ("cac".to_string(), "urn:cac".to_string()),
                ("udt".to_string(), "urn:udt".to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn synth_ns(pairs: &[(&str, &str)], ns: &NamespaceConfig) -> SourceModelMeta {
        let (model, _, diags) = synthesize_source_model_with(
            &nodes(pairs),
            "Invoice",
            "ubl:2.1",
            ns,
            &CodecTable::new(),
        );
        assert!(diags.is_empty(), "unexpected synth diagnostics: {diags:?}");
        model
    }

    /// A codec table holding the CII `102` date codec (wire `@format = 102`)
    /// and a wire-less ISO date codec.
    fn codecs() -> CodecTable {
        crate::codec::parse_codecs(
            r#"
            [codec.cii-date-102]
            for_type = "date"
            lexical = "YYYYMMDD"
            wire = { "@format" = "102" }

            [codec.date-iso]
            for_type = "date"
            lexical = "YYYY-MM-DD"
            "#,
        )
        .expect("codecs parse")
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect()
    }

    fn synth_codecs(
        pairs: &[(&str, &str)],
    ) -> (SourceModelMeta, BTreeMap<NodeId, String>, Vec<Diagnostic>) {
        synthesize_source_model_with(
            &nodes(pairs),
            "Invoice",
            "cii:1",
            &NamespaceConfig::default(),
            &codecs(),
        )
    }

    #[test]
    fn test_synth_codec_wire_attribute_makes_a_valued_container() {
        let (model, paths, diags) = synth_codecs(&[(
            "Invoice.IssueDateTime.DateTimeString",
            r#"type = "date"
            codec = "cii-date-102""#,
        )]);
        assert!(diags.is_empty(), "{diags:?}");
        let dts = &model.structs["IssueDateTimeDateTimeString"];
        assert_eq!(dts.fields["value"].xml.as_deref(), Some("$text"));
        let format = &dts.fields["format"];
        assert_eq!(format.xml.as_deref(), Some("@format"));
        assert!(format.optional && format.is_attribute());
        assert_eq!(
            paths[&NodeId::new("Invoice.IssueDateTime.DateTimeString")],
            "issue_date_time.date_time_string.value"
        );
        assert_eq!(
            ordered(&model, "IssueDateTimeDateTimeString"),
            ["format", "value"]
        );
    }

    #[test]
    fn test_synth_codec_without_wire_stays_a_plain_leaf() {
        let (model, paths, diags) = synth_codecs(&[(
            "Invoice.IssueDate",
            r#"type = "date"
            codec = "date-iso""#,
        )]);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(model.structs["Invoice"].fields["issue_date"].ty, Scalar);
        assert_eq!(paths[&NodeId::new("Invoice.IssueDate")], "issue_date");
    }

    #[test]
    fn test_synth_unknown_codec_is_left_to_the_validator() {
        // Synthesis places the node as if it had no codec; E084 is validate's.
        let (model, _, diags) = synth_codecs(&[(
            "Invoice.IssueDate",
            r#"type = "date"
            codec = "nope""#,
        )]);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(model.structs["Invoice"].fields["issue_date"].ty, Scalar);
    }

    #[test]
    fn test_synth_wire_attribute_colliding_with_attribute_node_is_e087() {
        let (_, _, diags) = synth_codecs(&[
            (
                "Invoice.IssueDateTime.DateTimeString",
                r#"type = "date"
                codec = "cii-date-102""#,
            ),
            (
                "Invoice.IssueDateTime.DateTimeString.format",
                r#"xml = "@format"
                type = "string""#,
            ),
        ]);
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E087" && d.message.contains("@format")),
            "{diags:?}"
        );
    }

    fn prefix_of<'a>(model: &'a SourceModelMeta, st: &str, field: &str) -> &'a str {
        &model.structs[st].fields[field].prefix
    }

    #[test]
    fn test_synth_default_config_leaves_everything_unprefixed() {
        let (model, _) = synth(&[("Invoice.Totals.Amount", r#"type = "decimal""#)]);
        assert_eq!(prefix_of(&model, "Invoice", "totals"), "");
        assert_eq!(prefix_of(&model, "Totals", "amount"), "");
        assert_eq!(model.namespaces, NamespaceMeta::default());
    }

    #[test]
    fn test_synth_prefixes_leaves_and_aggregates_by_default() {
        let model = synth_ns(
            &[
                ("Invoice.ID", r#"type = "identifier""#),
                (
                    "Invoice.LegalMonetaryTotal.PayableAmount",
                    r#"type = "decimal""#,
                ),
                (
                    "InvoiceLine",
                    r#"type = "collection"
                    canonical_key = "Lines""#,
                ),
                ("InvoiceLine.Item.Name", r#"type = "string""#),
            ],
            &ubl_ns(),
        );
        assert_eq!(prefix_of(&model, "Invoice", "id"), "cbc");
        assert_eq!(prefix_of(&model, "Invoice", "legal_monetary_total"), "cac");
        assert_eq!(
            prefix_of(&model, "LegalMonetaryTotal", "payable_amount"),
            "cbc"
        );
        assert_eq!(prefix_of(&model, "Invoice", "invoice_line"), "cac");
        assert_eq!(prefix_of(&model, "InvoiceLine", "item"), "cac");
        assert_eq!(prefix_of(&model, "InvoiceLineItem", "name"), "cbc");
        assert_eq!(model.namespaces.root_prefix, "");
        assert_eq!(model.namespaces.declared.len(), 4);
    }

    #[test]
    fn test_synth_valued_container_is_a_leaf_element_from_both_paths() {
        // `PayableAmount` is created by its own node *and* by the attribute's
        // path; both must agree on the leaf prefix or E024 would fire.
        let model = synth_ns(
            &[
                ("Invoice.Totals.PayableAmount", r#"type = "decimal""#),
                (
                    "Invoice.Totals.PayableAmount.currencyID",
                    r#"xml = "@currencyID"
                    type = "currency""#,
                ),
            ],
            &ubl_ns(),
        );
        assert_eq!(prefix_of(&model, "Totals", "payable_amount"), "cbc");
        assert_eq!(prefix_of(&model, "TotalsPayableAmount", "currency_id"), "");
        assert_eq!(prefix_of(&model, "TotalsPayableAmount", "value"), "");
    }

    #[test]
    fn test_synth_node_ns_overrides_default_and_structural_node_prefixes_interior() {
        let model = synth_ns(
            &[
                ("Invoice.Wrapper", r#"ns = "udt""#),
                (
                    "Invoice.Wrapper.IssueDate.DateTimeString",
                    r#"type = "date"
                    ns = "udt""#,
                ),
                (
                    "Invoice.Lines",
                    r#"type = "collection"
                    canonical_key = "Lines"
                    ns = "udt""#,
                ),
                ("Invoice.Lines.ID", r#"type = "identifier""#),
            ],
            &ubl_ns(),
        );
        assert_eq!(
            prefix_of(&model, "Invoice", "wrapper"),
            "udt",
            "structural node"
        );
        assert_eq!(
            prefix_of(&model, "Wrapper", "issue_date"),
            "cac",
            "inferred interior"
        );
        assert_eq!(
            prefix_of(&model, "WrapperIssueDate", "date_time_string"),
            "udt",
            "node ns"
        );
        assert_eq!(
            prefix_of(&model, "Invoice", "lines"),
            "udt",
            "collection ns"
        );
    }

    #[test]
    fn test_synth_required_structural_node_marks_interior_always_present() {
        let (model, _) = synth(&[
            ("Invoice.Delivery", "required = true"),
            ("Invoice.Delivery.ActualDeliveryDate", r#"type = "date""#),
            ("Invoice.Totals.Amount", r#"type = "decimal""#),
        ]);
        assert!(model.structs["Invoice"].fields["delivery"].always_present);
        assert!(!model.structs["Invoice"].fields["totals"].always_present);
        assert!(!model.structs["Delivery"].fields["actual_delivery_date"].always_present);
    }

    #[test]
    fn test_synth_undeclared_prefix_is_e080() {
        let (_, _, diags) = synthesize_source_model_with(
            &nodes(&[(
                "Invoice.ID",
                r#"type = "identifier"
                ns = "nope""#,
            )]),
            "Invoice",
            "ubl:2.1",
            &ubl_ns(),
            &CodecTable::new(),
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E080" && d.message.contains("nope")),
            "{diags:?}"
        );

        let mut bad_meta = ubl_ns();
        bad_meta.root_prefix = "rsm".into();
        bad_meta.aggregate_prefix = "ram".into();
        let (_, _, diags) = synthesize_source_model_with(
            &nodes(&[("Invoice.ID", r#"type = "identifier""#)]),
            "Invoice",
            "ubl:2.1",
            &bad_meta,
            &CodecTable::new(),
        );
        let e080: Vec<&str> = diags
            .iter()
            .filter(|d| d.code == "E080")
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(e080.len(), 2, "{diags:?}");
        assert!(
            e080.iter()
                .any(|m| m.contains("root_ns") && m.contains("rsm"))
        );
        assert!(
            e080.iter()
                .any(|m| m.contains("ns_defaults.aggregate") && m.contains("ram"))
        );
    }

    #[test]
    fn test_synth_empty_prefix_needs_no_declaration() {
        let mut ns = ubl_ns();
        ns.declared.remove("");
        let (_, _, diags) = synthesize_source_model_with(
            &nodes(&[("Invoice.ID", "type = \"identifier\"\nns = \"\"")]),
            "Invoice",
            "ubl:2.1",
            &ns,
            &CodecTable::new(),
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn test_synth_ns_on_attribute_or_text_is_e081() {
        for body in [
            "xml = \"@currencyID\"\ntype = \"currency\"\nns = \"cbc\"",
            "xml = \"$text\"\ntype = \"decimal\"\nns = \"cbc\"",
        ] {
            let (_, _, diags) = synthesize_source_model_with(
                &nodes(&[("Invoice.Amount.Leaf", body)]),
                "Invoice",
                "ubl:2.1",
                &ubl_ns(),
                &CodecTable::new(),
            );
            assert!(diags.iter().any(|d| d.code == "E081"), "{body}: {diags:?}");
        }
    }

    #[test]
    fn test_synth_dangling_or_root_structural_node_is_e083() {
        let (_, _, diags) = synthesize_source_model_with(
            &nodes(&[
                ("Invoice.Nothing", r#"ns = "cac""#),
                ("Invoice.ID", r#"type = "identifier""#),
            ]),
            "Invoice",
            "ubl:2.1",
            &ubl_ns(),
            &CodecTable::new(),
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E083" && d.message.contains("Invoice.Nothing")),
            "{diags:?}"
        );
        let (_, _, diags) = synthesize_source_model_with(
            &nodes(&[
                ("Invoice", r#"ns = "cac""#),
                ("Invoice.ID", r#"type = "identifier""#),
            ]),
            "Invoice",
            "ubl:2.1",
            &ubl_ns(),
            &CodecTable::new(),
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E083" && d.message.contains("root_ns")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_conflicting_field_is_e024() {
        // Two element names collapse to the same snake field but bind different
        // XML names (`ID` vs `Id`): the synthesized `id` field is ambiguous.
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[
                ("Invoice.ID", r#"type = "identifier""#),
                ("Invoice.Id", r#"type = "string""#),
            ]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(diags.iter().any(|d| d.code == "E024"), "{diags:?}");
    }

    /// Three logical nodes on CII's `AdditionalReferencedDocument`: tender (50)
    /// and object (130) as structural nodes, supporting documents (916) as the
    /// collection — all declared with the physical element's children.
    const REFERENCED_DOCUMENTS: &[(&str, &str)] = &[
        (
            "Invoice.Agreement.TenderDocument",
            r#"xml = "AdditionalReferencedDocument"
            match = { "TypeCode" = "50" }"#,
        ),
        (
            "Invoice.Agreement.TenderDocument.IssuerAssignedID",
            r#"type = "identifier"
            canonical_key = "TenderOrLotReference""#,
        ),
        (
            "Invoice.Agreement.TenderDocument.TypeCode",
            r#"type = "string""#,
        ),
        (
            "Invoice.Agreement.ObjectDocument",
            r#"xml = "AdditionalReferencedDocument"
            match = { "TypeCode" = "130" }"#,
        ),
        (
            "Invoice.Agreement.ObjectDocument.IssuerAssignedID",
            r#"type = "identifier"
            canonical_key = "InvoicedObjectIdentifier""#,
        ),
        (
            "Invoice.Agreement.AdditionalReferencedDocument",
            r#"type = "collection"
            canonical_key = "SupportingDocuments"
            match = { "TypeCode" = "916" }"#,
        ),
        (
            "Invoice.Agreement.AdditionalReferencedDocument.IssuerAssignedID",
            r#"type = "identifier"
            canonical_key = "SupportingDocumentReference""#,
        ),
        (
            "Invoice.Agreement.AdditionalReferencedDocument.Name",
            r#"type = "string"
            canonical_key = "SupportingDocumentDescription""#,
        ),
    ];

    #[test]
    fn test_synth_aliased_element_has_one_physical_and_one_logical_field_per_node() {
        let (model, paths) = synth(REFERENCED_DOCUMENTS);
        let agreement = &model.structs["Agreement"];
        let physical = &agreement.fields["all_additional_referenced_document"];
        assert!(physical.repeated && !physical.is_logical());
        assert_eq!(
            physical.xml.as_deref(),
            Some("AdditionalReferencedDocument")
        );
        let item = "AgreementAdditionalReferencedDocument";
        assert_eq!(physical.ty, Struct(item.into()));

        let tender = &agreement.fields["tender_document"];
        assert!(!tender.repeated && tender.is_logical());
        assert_eq!(
            tender.ty,
            Struct(item.into()),
            "logical nodes share the item struct"
        );
        assert_eq!(tender.xml, None, "never on the wire");
        let binding = tender.alias.as_ref().unwrap();
        assert_eq!(binding.physical, "all_additional_referenced_document");
        assert_eq!(binding.node, "Invoice.Agreement.TenderDocument");
        assert_eq!(
            binding.selector,
            [("type_code".to_string(), "50".to_string())],
            "selector keys are resolved to item field paths"
        );

        let supporting = &agreement.fields["additional_referenced_document"];
        assert!(supporting.repeated && supporting.is_logical());
        assert_eq!(
            supporting.alias.as_ref().unwrap().selector,
            [("type_code".to_string(), "916".to_string())]
        );

        // The item struct is the union of every logical node's children.
        assert_eq!(
            ordered(&model, item),
            ["issuer_assigned_id", "type_code", "name"]
        );
        // Paths run through the logical fields.
        assert_eq!(
            paths[&NodeId::new("Invoice.Agreement.TenderDocument.IssuerAssignedID")],
            "agreement.tender_document.issuer_assigned_id"
        );
        assert_eq!(
            paths[&NodeId::new("Invoice.Agreement.AdditionalReferencedDocument")],
            "agreement.additional_referenced_document"
        );
        assert_eq!(
            paths[&NodeId::new("Invoice.Agreement.AdditionalReferencedDocument.Name")],
            "name"
        );
        // The physical field sits where the first logical node's children are.
        assert_eq!(
            ordered(&model, "Agreement"),
            [
                "all_additional_referenced_document",
                "tender_document",
                "object_document",
                "additional_referenced_document"
            ]
        );
        let groups = agreement.aliased_fields();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 3);
        assert!(
            resolve_path(&model, "agreement.tender_document.issuer_assigned_id").is_ok(),
            "paths through a logical field resolve"
        );
    }

    #[test]
    fn test_synth_default_bucket_and_nested_selector_keys() {
        // UBL: the plain collection takes what the `130` node leaves; the
        // selector names a nested valued element and an attribute.
        let (model, _) = synth(&[
            (
                "Invoice.AdditionalDocumentReference",
                r#"type = "collection"
                canonical_key = "SupportingDocuments""#,
            ),
            (
                "Invoice.AdditionalDocumentReference.ID",
                r#"type = "identifier"
                canonical_key = "SupportingDocumentReference""#,
            ),
            (
                "Invoice.InvoicedObjectReference",
                r#"xml = "AdditionalDocumentReference"
                match = { "DocumentTypeCode" = "130", "ID.@schemeID" = "ABZ", "Scheme.ID" = "S" }"#,
            ),
            (
                "Invoice.InvoicedObjectReference.ID",
                r#"type = "identifier"
                canonical_key = "InvoicedObjectIdentifier""#,
            ),
            (
                "Invoice.InvoicedObjectReference.ID.schemeID",
                r#"xml = "@schemeID"
                type = "string""#,
            ),
            (
                "Invoice.InvoicedObjectReference.DocumentTypeCode",
                r#"type = "string""#,
            ),
            (
                "Invoice.InvoicedObjectReference.Scheme.ID",
                r#"type = "string""#,
            ),
        ]);
        let inv = &model.structs["Invoice"];
        let default = inv.fields["additional_document_reference"]
            .alias
            .as_ref()
            .unwrap();
        assert!(
            default.selector.is_empty(),
            "no selector: the default bucket"
        );
        let object = inv.fields["invoiced_object_reference"]
            .alias
            .as_ref()
            .unwrap();
        assert_eq!(
            object.selector,
            [
                ("document_type_code".to_string(), "130".to_string()),
                ("id.scheme_id".to_string(), "ABZ".to_string()),
                ("scheme.id".to_string(), "S".to_string()),
            ]
        );
        // `ID` became a valued container because of the attribute declared
        // under one logical node; the other node's `ID` leaf follows suit, so
        // both land on the same union field.
        assert_eq!(
            inv.fields["all_additional_document_reference"].ty,
            Struct("AdditionalDocumentReference".into())
        );
        assert_eq!(
            model.structs["AdditionalDocumentReference"].fields["id"].ty,
            Struct("AdditionalDocumentReferenceID".into())
        );
    }

    #[test]
    fn test_synth_single_matched_structural_node_is_aliased_alone() {
        let (model, paths) = synth(&[
            (
                "Invoice.Party.PartyTaxScheme",
                r#"match = { "TaxScheme.ID" = "VAT" }"#,
            ),
            (
                "Invoice.Party.PartyTaxScheme.CompanyID",
                r#"type = "identifier"
                canonical_key = "SellerVatIdentifier""#,
            ),
            (
                "Invoice.Party.PartyTaxScheme.TaxScheme.ID",
                r#"type = "identifier"
                constant = "VAT""#,
            ),
        ]);
        let party = &model.structs["Party"];
        assert!(party.fields["all_party_tax_scheme"].repeated);
        let logical = &party.fields["party_tax_scheme"];
        assert!(logical.is_logical() && !logical.repeated);
        assert_eq!(
            logical.alias.as_ref().unwrap().selector,
            [("tax_scheme.id".to_string(), "VAT".to_string())]
        );
        assert_eq!(
            paths[&NodeId::new("Invoice.Party.PartyTaxScheme.CompanyID")],
            "party.party_tax_scheme.company_id"
        );
    }

    #[test]
    fn test_synth_rename_only_structural_node_is_a_plain_rename() {
        let (model, paths) = synth(&[
            ("Invoice.Wrapper", r#"xml = "Envelope""#),
            ("Invoice.Wrapper.ID", r#"type = "identifier""#),
        ]);
        let wrapper = &model.structs["Invoice"].fields["wrapper"];
        assert!(!wrapper.is_logical() && !wrapper.repeated);
        assert_eq!(wrapper.xml.as_deref(), Some("Envelope"));
        assert_eq!(
            wrapper.ty,
            Struct("Envelope".into()),
            "struct named physically"
        );
        assert_eq!(paths[&NodeId::new("Invoice.Wrapper.ID")], "wrapper.id");
    }

    #[test]
    fn test_synth_match_on_scalar_is_e091() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[(
                "Invoice.ID",
                r#"type = "identifier"
                match = { "x" = "y" }"#,
            )]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(diags.iter().any(|d| d.code == "E091"), "{diags:?}");
    }

    #[test]
    fn test_synth_overlapping_selectors_are_e090() {
        let base = |sel_a: &str, sel_b: &str| {
            let a = format!("xml = \"Ref\"\n{sel_a}");
            let b = format!("xml = \"Ref\"\n{sel_b}");
            let (_, _, diags) = synthesize_source_model(
                &nodes(&[
                    ("Invoice.A", a.as_str()),
                    ("Invoice.A.TypeCode", r#"type = "string""#),
                    ("Invoice.A.Kind", r#"type = "string""#),
                    ("Invoice.B", b.as_str()),
                    ("Invoice.B.TypeCode", r#"type = "string""#),
                ]),
                "Invoice",
                "ubl:2.1",
            );
            diags
                .iter()
                .filter(|d| d.code == "E090")
                .map(|d| d.message.clone())
                .collect::<Vec<_>>()
        };
        assert!(
            base(
                r#"match = { "TypeCode" = "50" }"#,
                r#"match = { "TypeCode" = "130" }"#
            )
            .is_empty()
        );
        assert!(
            base("", r#"match = { "TypeCode" = "130" }"#).is_empty(),
            "default + selector"
        );
        let same = base(
            r#"match = { "TypeCode" = "50" }"#,
            r#"match = { "TypeCode" = "50" }"#,
        );
        assert_eq!(same.len(), 1, "{same:?}");
        assert!(same[0].contains("Invoice.A") && same[0].contains("Invoice.B"));
        let subset = base(
            r#"match = { "TypeCode" = "50" }"#,
            r#"match = { "TypeCode" = "50", "Kind" = "x" }"#,
        );
        assert_eq!(subset.len(), 1, "{subset:?}");
        let disjoint_keys = base(
            r#"match = { "Kind" = "x" }"#,
            r#"match = { "TypeCode" = "50" }"#,
        );
        assert_eq!(disjoint_keys.len(), 1, "different keys can co-occur");
        let two_defaults = base("", "");
        assert_eq!(two_defaults.len(), 1, "{two_defaults:?}");
        assert!(two_defaults[0].contains("neither has a `match`"));
    }

    #[test]
    fn test_synth_unresolvable_selector_key_is_e092() {
        let run = |sel: &str| {
            let a = format!("xml = \"Ref\"\nmatch = {sel}");
            let (_, _, diags) = synthesize_source_model(
                &nodes(&[
                    ("Invoice.A", a.as_str()),
                    ("Invoice.A.TypeCode", r#"type = "string""#),
                    (
                        "Invoice.A.Notes",
                        r#"type = "string"
                        multiple = "first""#,
                    ),
                    ("Invoice.A.Inner.Leaf", r#"type = "string""#),
                ]),
                "Invoice",
                "ubl:2.1",
            );
            diags
                .iter()
                .filter(|d| d.code == "E092")
                .map(|d| d.message.clone())
                .collect::<Vec<_>>()
        };
        assert!(run(r#"{ "TypeCode" = "1" }"#).is_empty());
        assert!(run(r#"{ "Inner.Leaf" = "1" }"#).is_empty());
        assert_eq!(run(r#"{ "Missing" = "1" }"#).len(), 1, "undeclared child");
        assert_eq!(run(r#"{ "Notes" = "1" }"#).len(), 1, "repeated leaf");
        assert_eq!(
            run(r#"{ "Inner" = "1" }"#).len(),
            1,
            "container without text"
        );
        assert_eq!(
            run(r#"{ "TypeCode.Deeper" = "1" }"#).len(),
            1,
            "below a value"
        );
        assert_eq!(run("{ }").len(), 1, "empty selector");
    }

    #[test]
    fn test_synth_structural_node_bound_to_attribute_is_e083() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[
                ("Invoice.A", r#"xml = "@attr""#),
                ("Invoice.A.ID", r#"type = "string""#),
            ]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E083" && d.message.contains("attribute")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_struct_names_are_unique_per_element_path() {
        // Two `TaxCategory` elements under different parents get distinct
        // structs, each ordered by its own declarations.
        let (model, _) = synth(&[
            (
                "Invoice.AllowanceCharge",
                r#"type = "collection"
                canonical_key = "Charges""#,
            ),
            (
                "Invoice.AllowanceCharge.TaxCategory.ID",
                r#"type = "string""#,
            ),
            (
                "InvoiceLine",
                r#"type = "collection"
                canonical_key = "Lines""#,
            ),
            (
                "InvoiceLine.Item.TaxCategory.Percent",
                r#"type = "decimal""#,
            ),
            ("InvoiceLine.Item.TaxCategory.ID", r#"type = "string""#),
        ]);
        assert_eq!(ordered(&model, "AllowanceChargeTaxCategory"), ["id"]);
        assert_eq!(
            ordered(&model, "InvoiceLineItemTaxCategory"),
            ["percent", "id"]
        );
        assert!(!model.structs.contains_key("TaxCategory"));
        assert_eq!(
            struct_name_for(&NodeId::new("Invoice.A.B"), "Invoice"),
            "AB"
        );
        assert_eq!(
            struct_name_for(&NodeId::new("InvoiceLine"), "Invoice"),
            "InvoiceLine"
        );
        assert_eq!(
            struct_name_for(&NodeId::new("Invoice"), "Invoice"),
            "Invoice"
        );
    }
}
