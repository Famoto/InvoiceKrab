//! Writer generation: `write(mut main: MainKey) -> MappingResult<Root>`.
//!
//! The inverse of the reader. Constructs a `Default` source `Root` and assigns
//! each canonical field back to its primary `source_path`, rendering the typed
//! value to its source `String` form. Fallbacks and helper nodes are skipped.
//!
//! A node with a `constant` is written from that literal instead of the hub,
//! but only where the schema would otherwise see a hole: its *owner* — the
//! deepest interior element on its path that other mapped nodes also write
//! into — must be non-empty (so `PartyTaxScheme/TaxScheme/ID = "VAT"` appears
//! exactly when the party has a `PartyTaxScheme/CompanyID`). A constant with
//! no such owner — or whose owner is an always-present element (below) — is
//! written unconditionally at root and, inside a collection, on every
//! non-empty item, so a constant never resurrects an otherwise-empty element. Constants are written last in their scope, after everything that
//! could fill their owner.
//!
//! A structural node with `required = true` names an interior element the
//! writer always materializes, even empty, for schemas that make it mandatory
//! (CII's `ApplicableHeaderTradeDelivery`).
//!
//! A `clone_of` node fans its target key's hub value out to a second source
//! path — how a format stores one canonical value in several places (currency
//! attributes, duplicated VAT ids). `$parent.Key` and `$root.Key` clones read
//! the enclosing scope's / the root's hub value instead, which therefore stays
//! a borrow (its primary never moves it).
//!
//! An attribute of a *valued* element (`PayableAmount/@currencyID`) is written
//! only when the element has its text value, so an absent amount never shows up
//! as an empty element carrying just its currency. Such attributes are written
//! after every other scalar and clone of their scope, so the guard sees the
//! element's text whether a primary or a clone supplies it.
//!
//! A node with a `codec` renders the canonical value through the codec's
//! encoder and sets the codec's wire attributes on the element next to it
//! (`<DateTimeString format="102">20260718</DateTimeString>`).
//!
//! When the mapping aliases physical elements (`match` selectors), every node
//! writes into its logical field, and the finished source is `mux`ed last:
//! each logical field's items move into the physical element in declaration
//! order, with the selector values written as discriminators.
//!
//! The writer **consumes** the hub: canonical fields written exactly once move
//! their values into the target struct (`take`); a key written from more than
//! one node (a primary plus its clones) stays a borrow + clone.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::codec::{Codec, Pattern};
use crate::node::{DerivationScope, Scope, SourceNode};
use crate::source_model::{FieldType, SourceModelMeta, xml_field_name};
use crate::types::MappingType;

use super::access::{assign_target_expr, collection_item_struct, walk_segments};
use super::diag::DiagSpec;
use super::naming::snake_case;
use super::plan::{Frame, GenCtx};

/// The canonical keys written more than once within one scope: a `clone_of`
/// node fans its target key out to a second source path, so the key is read
/// from the hub twice, and `derived` are the keys clones in deeper scopes read
/// through `$parent` / `$root`. These must stay borrow + clone reads of the
/// hub; unique keys move out. Nodes with a `constant` never read the hub, and
/// a clone reading another scope's key does not count here, so neither counts.
fn shared_hub_keys(
    scalars: &[&SourceNode],
    clones: &[&SourceNode],
    collections: &[&SourceNode],
    derived: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut shared: BTreeSet<String> = derived.clone();
    for node in scalars.iter().chain(clones).chain(collections) {
        if node.constant.is_some() || derivation_scope(node) != DerivationScope::Own {
            continue;
        }
        let key = hub_key(node);
        if !seen.insert(key) {
            shared.insert(key.to_string());
        }
    }
    shared
}

/// The hub key a node writes from: its own `canonical_key`, or the mirrored
/// key (scope prefix stripped) for a `clone_of` node.
fn hub_key(node: &SourceNode) -> &str {
    match node.derivation() {
        Some(Ok(d)) => d.key,
        Some(Err(_)) => node.clone_of.as_deref().expect("clone node"),
        None => node.canonical_key.as_deref().expect("mapped node"),
    }
}

/// Which scope a node reads its hub value from (`Own` for every non-clone).
fn derivation_scope(node: &SourceNode) -> DerivationScope {
    match node.derivation() {
        Some(Ok(d)) => d.scope,
        _ => DerivationScope::Own,
    }
}

/// The hub variables a write block can read from: its own scope's, and the
/// enclosing scope's for `$parent` clones (`None` at root). `$root` clones
/// always read `main`.
struct HubVars<'a> {
    own: &'a str,
    parent: Option<&'a str>,
}

/// Emits the `write(mut main: MainKey) -> MappingResult<Root>` function into
/// `out`: the inverse of the reader. It constructs a `Default` source `Root` and
/// assigns each canonical field back to its primary `source_path`. Fallbacks and
/// helper nodes are skipped.
pub(super) fn generate_write(out: &mut String, ctx: &GenCtx, root: &str) {
    out.push_str("/// Writes the canonical hub back into a typed source document, consuming\n");
    out.push_str("/// the hub so uniquely-written values move instead of clone.\n");
    let _ = writeln!(
        out,
        "pub fn write(mut main: MainKey) -> MappingResult<{root}> {{"
    );
    out.push_str("    let mut diagnostics: Vec<MappingDiagnostic> = Vec::new();\n");
    let _ = writeln!(out, "    let mut source = {root}::default();");

    let shared = shared_hub_keys(
        &ctx.plan.root_scalars,
        &ctx.plan.root_clones,
        &ctx.plan.root_collections,
        ctx.plan.derived_keys(&Scope::Root),
    );
    let root_vars = HubVars {
        own: "main",
        parent: None,
    };
    for node in scalar_write_order(
        ctx.source,
        &ctx.source.root,
        &ctx.plan.root_scalars,
        &ctx.plan.root_clones,
    ) {
        out.push('\n');
        write_scalar_block(
            out,
            ctx,
            node,
            &ctx.source.root,
            &root_vars,
            "source",
            None,
            1,
            !shared.contains(hub_key(node)),
        );
    }

    for coll in &ctx.plan.root_collections {
        out.push('\n');
        write_collection_block(
            out,
            ctx,
            coll,
            &Frame {
                depth: 0,
                indent: 1,
                parent_hub: "main",
                parent_src: "source",
                parent_struct: &ctx.source.root,
                owned: true,
            },
            &shared,
        );
    }

    // Constants and always-present elements go last: their owner guards look
    // at what every other node in the scope has written.
    let root_content = scope_content_paths(
        &ctx.plan.root_scalars,
        &ctx.plan.root_clones,
        &ctx.plan.root_collections,
    );
    for node in &ctx.plan.root_constants {
        out.push('\n');
        write_constant_block(
            out,
            ctx.source,
            node,
            &ctx.source.root,
            "source",
            1,
            &root_content,
        );
    }
    write_always_present(out, ctx.source, &ctx.source.root, "source", 1);
    if super::source::root_has_alias_io(ctx.source) {
        // Aliased elements: move every logical field's items back into the
        // physical element, with the selector values as discriminators.
        out.push('\n');
        out.push_str("    source.mux();\n");
    }

    out.push('\n');
    out.push_str("    MappingResult::new(Some(source), diagnostics)\n");
    out.push_str("}\n");
}

/// The source paths of every node in a scope that writes real content
/// (primaries, clones, collections — not constants): what a constant's owner
/// guard is measured against.
fn scope_content_paths<'a>(
    scalars: &[&'a SourceNode],
    clones: &[&'a SourceNode],
    collections: &[&'a SourceNode],
) -> Vec<&'a str> {
    scalars
        .iter()
        .chain(clones)
        .chain(collections)
        .filter(|n| n.constant.is_none())
        .map(|n| n.source_path.as_str())
        .collect()
}

/// Emits, for every interior element under `start_struct` flagged
/// `always_present` (a structural node with `required = true`), the
/// materialization that makes the writer emit it even when empty.
fn write_always_present(
    out: &mut String,
    source: &SourceModelMeta,
    start_struct: &str,
    src_var: &str,
    indent: usize,
) {
    let pad = "    ".repeat(indent);
    for path in always_present_paths(source, start_struct, "") {
        let _ = writeln!(out, "{pad}// always present -> {path}");
        let _ = writeln!(
            out,
            "{pad}let _ = {}.get_or_insert_default();",
            assign_target_expr(source, start_struct, &path, src_var)
        );
    }
}

/// The dotted paths (from `start_struct`, through non-repeated interior
/// structs only) of every field flagged `always_present`, in emission order.
fn always_present_paths(source: &SourceModelMeta, start_struct: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(meta) = source.structs.get(start_struct) else {
        return out;
    };
    for (name, field) in meta.ordered_fields() {
        let FieldType::Struct(inner) = &field.ty else {
            continue;
        };
        if field.repeated {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if field.always_present {
            out.push(path.clone());
        }
        out.extend(always_present_paths(source, inner, &path));
    }
    out
}

/// Emits the write loop for one collection node: builds a source element per hub
/// item, writes the scalar children, recurses into nested collections, and pushes
/// the element. Mirrors `read_collection_block`: a uniquely-written hub
/// collection is consumed (`mem::take` + `into_iter`); a shared one is iterated
/// by reference and its subtree clones.
fn write_collection_block(
    out: &mut String,
    ctx: &GenCtx,
    coll: &SourceNode,
    frame: &Frame,
    shared: &BTreeSet<String>,
) {
    let coll_key = coll.canonical_key.as_deref().expect("mapped collection");
    let src_item_struct =
        collection_item_struct(ctx.source, frame.parent_struct, &coll.source_path);
    let children = ctx.plan.children_of(&coll.id);
    let nested = ctx.plan.nested_collections_of(&coll.id);
    let clones = ctx.plan.clones_of(&coll.id);
    let owned = frame.owned && !shared.contains(coll_key);
    let child_shared = shared_hub_keys(
        children,
        clones,
        nested,
        ctx.plan.derived_keys(&Scope::Collection(coll.id.clone())),
    );

    let depth = frame.depth;
    let indent = frame.indent;
    let parent_hub = frame.parent_hub;
    let parent_src = frame.parent_src;
    let elem = format!("element{depth}");
    let hub_item = format!("hub_item{depth}");
    let idx = format!("idx{depth}");
    let count = format!("written_count{depth}");
    let pad = "    ".repeat(indent);
    let body = "    ".repeat(indent + 1);
    let child_vars = HubVars {
        own: &hub_item,
        parent: Some(parent_hub),
    };

    let _ = writeln!(
        out,
        "{pad}// {coll_key} -> {} (collection)",
        coll.source_path
    );
    let _ = writeln!(out, "{pad}let mut {count} = 0usize;");
    // The hub item count caps how many source elements can be pushed, so reserve
    // the source collection once up front rather than regrowing it per item.
    // When the path crosses a boxed interior container, the reserve is guarded
    // so an empty hub collection never materializes the subtree.
    let target = assign_target_expr(
        ctx.source,
        frame.parent_struct,
        &coll.source_path,
        parent_src,
    );
    let crosses_boxed = walk_segments(ctx.source, frame.parent_struct, &coll.source_path)
        .is_ok_and(|segs| segs.iter().any(|s| s.optional));
    if owned {
        let hub_items = format!("hub_items{depth}");
        let _ = writeln!(
            out,
            "{pad}let {hub_items} = std::mem::take(&mut {parent_hub}.{});",
            snake_case(coll_key)
        );
        if crosses_boxed {
            let _ = writeln!(out, "{pad}if !{hub_items}.is_empty() {{");
            let _ = writeln!(out, "{pad}    {target}.reserve({hub_items}.len());");
            let _ = writeln!(out, "{pad}}}");
        } else {
            let _ = writeln!(out, "{pad}{target}.reserve({hub_items}.len());");
        }
        let _ = writeln!(
            out,
            "{pad}for ({idx}, mut {hub_item}) in {hub_items}.into_iter().enumerate() {{"
        );
    } else {
        let hub_len = format!("{parent_hub}.{}.len()", snake_case(coll_key));
        if crosses_boxed {
            let _ = writeln!(out, "{pad}if {hub_len} > 0 {{");
            let _ = writeln!(out, "{pad}    {target}.reserve({hub_len});");
            let _ = writeln!(out, "{pad}}}");
        } else {
            let _ = writeln!(out, "{pad}{target}.reserve({hub_len});");
        }
        let _ = writeln!(
            out,
            "{pad}for ({idx}, {hub_item}) in {parent_hub}.{}.iter().enumerate() {{",
            snake_case(coll_key)
        );
    }
    let _ = writeln!(out, "{body}let mut {elem} = {src_item_struct}::default();");
    for child in scalar_write_order(ctx.source, &src_item_struct, children, clones) {
        write_scalar_block(
            out,
            ctx,
            child,
            &src_item_struct,
            &child_vars,
            &elem,
            Some(&idx),
            indent + 1,
            owned && !child_shared.contains(hub_key(child)),
        );
    }
    for nested_coll in nested {
        write_collection_block(
            out,
            ctx,
            nested_coll,
            &Frame {
                depth: depth + 1,
                indent: indent + 1,
                parent_hub: &hub_item,
                parent_src: &elem,
                parent_struct: &src_item_struct,
                owned,
            },
            &child_shared,
        );
    }
    let _ = writeln!(out, "{body}if !{elem}.is_empty() {{");
    let content = scope_content_paths(children, clones, nested);
    for constant in ctx.plan.constants_of(&coll.id) {
        write_constant_block(
            out,
            ctx.source,
            constant,
            &src_item_struct,
            &elem,
            indent + 2,
            &content,
        );
    }
    write_always_present(out, ctx.source, &src_item_struct, &elem, indent + 2);
    let _ = writeln!(out, "{body}    {target}.push({elem});");
    let _ = writeln!(out, "{body}    {count} += 1;");
    let _ = writeln!(out, "{body}}}");
    let _ = writeln!(out, "{pad}}}");

    // A required collection must have written at least one item.
    if coll.required {
        let _ = writeln!(out, "{pad}if {count} == 0 {{");
        let msg = format!("\"required collection `{coll_key}` has no items\"");
        DiagSpec::new(
            "Severity::Error",
            "REQUIRED_MISSING",
            coll.id.as_str(),
            &msg,
        )
        .key(coll_key)
        .path(&coll.source_path)
        .emit(out, &body);
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Emits the assignment of a node's `constant` literal to its source path. The
/// hub is never consulted; the literal was validated against the node's `type`
/// at compile time (E061), so it is emitted verbatim. When the constant has an
/// *owner* — the deepest interior element on its path that one of
/// `scope_content` also writes into — the assignment is guarded on that owner
/// being non-empty, so the constant completes real content instead of
/// conjuring an element on its own — unless the owner is always present, in
/// which case there is always a hole to fill.
#[allow(clippy::too_many_arguments)]
fn write_constant_block(
    out: &mut String,
    source: &SourceModelMeta,
    node: &SourceNode,
    start_struct: &str,
    src_var: &str,
    indent: usize,
    scope_content: &[&str],
) {
    let lit = node.constant.as_deref().expect("constant node");
    let path = &node.source_path;
    let target = assign_target_expr(source, start_struct, path, src_var);
    let optional = walk_segments(source, start_struct, path)
        .unwrap_or_default()
        .last()
        .is_some_and(|s| s.optional);
    let value = format!("CompactString::from({lit:?})");
    let assign = if optional {
        format!("{target} = Some({value});")
    } else {
        format!("{target} = {value};")
    };

    let pad = "    ".repeat(indent);
    let _ = writeln!(out, "{pad}// constant -> {path}");
    // An always-present owner (a `required` structural element) exists in
    // every document, so the constant fills a hole that is always there.
    let owner = constant_owner(path, scope_content)
        .filter(|owner| !always_present_paths(source, start_struct, "").contains(owner));
    match owner {
        Some(owner) => {
            let owner_ref = struct_ref_expr(source, start_struct, &owner, src_var);
            let _ = writeln!(
                out,
                "{pad}if {owner_ref}.is_some_and(|owner| !owner.is_empty()) {{"
            );
            let _ = writeln!(out, "{pad}    {assign}");
            let _ = writeln!(out, "{pad}}}");
        }
        None => {
            let _ = writeln!(out, "{pad}{assign}");
        }
    }
}

/// The order a scope's scalars are written in: primaries (constants aside),
/// then clones — and the attributes of valued elements last of all, so their
/// guard inspects an element whose text has been written already, whichever
/// of a primary or a clone supplies it.
fn scalar_write_order<'a>(
    source: &SourceModelMeta,
    start_struct: &str,
    scalars: &[&'a SourceNode],
    clones: &[&'a SourceNode],
) -> Vec<&'a SourceNode> {
    let candidates = scalars
        .iter()
        .filter(|n| n.constant.is_none())
        .chain(clones)
        .copied();
    let (attributes, others): (Vec<&SourceNode>, Vec<&SourceNode>) = candidates
        .partition(|n| valued_element_parent(source, start_struct, &n.source_path).is_some());
    others.into_iter().chain(attributes).collect()
}

/// For an attribute leaf whose parent struct is a valued element (it carries a
/// `$text` `value` field), the path of that element; `None` for element leaves
/// and for attributes of pure containers.
fn valued_element_parent<'p>(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &'p str,
) -> Option<&'p str> {
    let (parent_path, leaf) = path.rsplit_once('.')?;
    let segs = walk_segments(source, start_struct, parent_path).ok()?;
    let parent_struct = segs.last()?.struct_name.as_deref()?;
    let parent = source.structs.get(parent_struct)?;
    let leaf_meta = parent.fields.get(leaf)?;
    if !leaf_meta.is_attribute() {
        return None;
    }
    let has_value = parent.fields.get("value").is_some_and(|f| f.is_text());
    has_value.then_some(parent_path)
}

/// For an attribute leaf of a valued element, the `Option<&Struct>` expression
/// reaching that element (see [`valued_element_parent`]).
fn valued_element_guard(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    src_var: &str,
) -> Option<String> {
    valued_element_parent(source, start_struct, path)
        .map(|parent_path| struct_ref_expr(source, start_struct, parent_path, src_var))
}

/// The owner of a constant at `path`: its longest proper path prefix that some
/// content path in the scope also lies under, or `None` when the constant
/// shares no interior element with real content.
fn constant_owner(path: &str, scope_content: &[&str]) -> Option<String> {
    let segs: Vec<&str> = path.split('.').collect();
    (1..segs.len()).rev().find_map(|len| {
        let prefix = segs[..len].join(".");
        let under = format!("{prefix}.");
        scope_content
            .iter()
            .any(|p| p.starts_with(&under))
            .then_some(prefix)
    })
}

/// The `Option<&Struct>` expression reaching the interior struct at `path`
/// without materializing anything: `Some(&src).and_then(|v0| v0.a.as_ref())…`.
fn struct_ref_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    src_var: &str,
) -> String {
    let Ok(segs) = walk_segments(source, start_struct, path) else {
        return format!(
            "compile_error!(\"unresolved owner path `{path}` against `{start_struct}`\")"
        );
    };
    let mut expr = format!("Some(&{src_var})");
    for (i, seg) in segs.iter().enumerate() {
        if seg.optional {
            expr = format!("{expr}.and_then(|v{i}| v{i}.{}.as_ref())", seg.name);
        } else {
            expr = format!("{expr}.map(|v{i}| &v{i}.{})", seg.name);
        }
    }
    expr
}

/// Emits the write of one canonical field back to its source path, rendering the
/// typed value to the source `String` representation (through the node's codec
/// when it has one, plus the codec's wire attributes). When `take`, the hub
/// value is moved out instead of cloned (the field is written exactly once).
#[allow(clippy::too_many_arguments)]
fn write_scalar_block(
    out: &mut String,
    ctx: &GenCtx,
    node: &SourceNode,
    start_struct: &str,
    hub_vars: &HubVars,
    src_var: &str,
    index_var: Option<&str>,
    indent: usize,
    take: bool,
) {
    let source = ctx.source;
    let codec = ctx.codec_of(node);
    let key = hub_key(node);
    // A `$parent` / `$root` clone reads another scope's hub value, which is
    // always a borrow: the owning scope writes the key itself.
    let (hub_var, take) = match derivation_scope(node) {
        DerivationScope::Own => (hub_vars.own, take),
        DerivationScope::Root => ("main", false),
        DerivationScope::Parent => (
            hub_vars
                .parent
                .expect("E093 rejects `$parent` at root scope before codegen"),
            false,
        ),
    };
    let path = &node.source_path;
    let field = snake_case(key);
    let segs = walk_segments(source, start_struct, path).unwrap_or_default();
    // Only the leaf's own optionality picks the assignment form; interior
    // containers are boxed-optional and materialized by the target chain.
    let optional = segs.last().is_some_and(|s| s.optional);
    // A `multiple` node's source field is `Vec<String>`: the writer pushes the
    // single canonical value as one element (a joined value stays joined).
    let repeated_leaf = segs
        .last()
        .is_some_and(|s| s.repeated && s.struct_name.is_none());
    // The mutable place to assign into: boxed interiors materialize on demand,
    // and only inside the non-empty guard below, so no empty subtree is built.
    let target = assign_target_expr(source, start_struct, path, src_var);
    // An attribute of a valued element follows the element: it is written only
    // when the element's own text value is present (the element struct is
    // non-empty before the attribute is added — attributes are written after
    // their element's value, in id order).
    let element_guard = valued_element_guard(source, start_struct, path, src_var);
    // Render the typed value to a source string. Decimals/booleans render
    // fresh (inline, no heap for short renderings); strings move when taken,
    // clone when the hub value is shared.
    let rendered = match node.source_type {
        MappingType::Decimal | MappingType::Boolean => "value.to_compact_string()",
        _ if take => "value",
        _ => "value.clone()",
    };

    let pad = "    ".repeat(indent);
    let _ = writeln!(out, "{pad}// {key} -> {path}");
    if take {
        let _ = writeln!(out, "{pad}if let Some(value) = {hub_var}.{field}.take() {{");
    } else {
        let _ = writeln!(out, "{pad}if let Some(value) = &{hub_var}.{field} {{");
    }
    if let Some(Codec {
        pattern: Pattern::Split { at, tail, .. },
        ..
    }) = codec
    {
        // The value's two parts go to the element's head and tail children.
        let tail_path = match path.rsplit_once('.') {
            Some((element, _)) => format!("{element}.{}", xml_field_name(tail)),
            None => xml_field_name(tail),
        };
        let tail_target = assign_target_expr(source, start_struct, &tail_path, src_var);
        let _ = writeln!(
            out,
            "{pad}    match codec::split_at(value.as_str(), {at}) {{"
        );
        let _ = writeln!(out, "{pad}        Some((head, tail)) => {{");
        let _ = writeln!(out, "{pad}            {target} = Some(head);");
        let _ = writeln!(out, "{pad}            {tail_target} = Some(tail);");
        let _ = writeln!(out, "{pad}        }}");
        let _ = writeln!(out, "{pad}        None => {{");
        let msg = format!(
            "format!(\"`{{value}}` cannot be encoded with codec `{}` ({})\")",
            codec.map_or("", |c| c.id.as_str()),
            codec.map_or("", |c| c.lexical.as_str())
        );
        DiagSpec::new("Severity::Error", "CODEC_INVALID", node.id.as_str(), &msg)
            .key(key)
            .path(path)
            .index(index_var)
            .emit(out, &format!("{pad}            "));
        let _ = writeln!(out, "{pad}        }}");
        let _ = writeln!(out, "{pad}    }}");
        if node.required {
            let _ = writeln!(out, "{pad}}} else {{");
            emit_required_missing(out, node, key, path, index_var, &format!("{pad}    "));
        }
        let _ = writeln!(out, "{pad}}}");
        return;
    }
    match codec {
        Some(codec) => write_encoded(out, node, codec, key, index_var, &format!("{pad}    ")),
        None => {
            let _ = writeln!(out, "{pad}    let rendered = {rendered};");
        }
    }
    match &element_guard {
        Some(guard) => {
            let _ = writeln!(
                out,
                "{pad}    if !rendered.is_empty() && {guard}.is_some_and(|owner| !owner.is_empty()) {{"
            );
        }
        None => {
            let _ = writeln!(out, "{pad}    if !rendered.is_empty() {{");
        }
    }
    if repeated_leaf {
        let _ = writeln!(out, "{pad}        {target}.push(rendered);");
    } else if optional {
        let _ = writeln!(out, "{pad}        {target} = Some(rendered);");
    } else {
        let _ = writeln!(out, "{pad}        {target} = rendered;");
    }
    // The codec's wire attributes sit on the element the value was just
    // written into, so they are set inside the same non-empty guard.
    if let (Some(codec), Some(element_path)) = (codec, path.strip_suffix(".value")) {
        for (attr, value) in &codec.wire {
            let attr_target = assign_target_expr(
                source,
                start_struct,
                &format!("{element_path}.{}", snake_case(attr)),
                src_var,
            );
            let _ = writeln!(
                out,
                "{pad}        {attr_target} = Some(CompactString::from({value:?}));"
            );
        }
    }
    if node.required {
        let _ = writeln!(out, "{pad}    }} else {{");
        emit_required_missing(out, node, key, path, index_var, &format!("{pad}        "));
        let _ = writeln!(out, "{pad}    }}");
    } else {
        let _ = writeln!(out, "{pad}    }}");
    }
    if node.required {
        let _ = writeln!(out, "{pad}}} else {{");
        emit_required_missing(out, node, key, path, index_var, &format!("{pad}    "));
        let _ = writeln!(out, "{pad}}}");
    } else {
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Emits `let rendered = …;` for a codec node: the canonical `value` encoded in
/// the codec's lexical form. A canonical value the encoder cannot render (which
/// the reader's own validation makes unreachable) is a `CODEC_INVALID`
/// diagnostic and an empty rendering, which the non-empty guard then skips.
fn write_encoded(
    out: &mut String,
    node: &SourceNode,
    codec: &Codec,
    key: &str,
    index_var: Option<&str>,
    pad: &str,
) {
    match (&codec.pattern, node.source_type) {
        (Pattern::Boolean { yes, no }, MappingType::Boolean) => {
            let _ = writeln!(
                out,
                "{pad}let rendered = codec::encode_bool(value.clone(), {yes:?}, {no:?});"
            );
        }
        (Pattern::Temporal(_), MappingType::Date | MappingType::Datetime) => {
            let func = if node.source_type == MappingType::Date {
                "encode_date"
            } else {
                "encode_datetime"
            };
            let _ = writeln!(
                out,
                "{pad}let rendered = match codec::{func}(value.as_str(), {:?}) {{",
                codec.lexical
            );
            let _ = writeln!(out, "{pad}    Some(s) => s,");
            let _ = writeln!(out, "{pad}    None => {{");
            let msg = format!(
                "format!(\"`{{value}}` cannot be encoded with codec `{}` ({})\")",
                codec.id, codec.lexical
            );
            DiagSpec::new("Severity::Error", "CODEC_INVALID", node.id.as_str(), &msg)
                .key(key)
                .path(&node.source_path)
                .index(index_var)
                .emit(out, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}        CompactString::new(\"\")");
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}};");
        }
        (Pattern::Values(pairs), _) => {
            // First pair per canonical value; an unmapped value cannot be
            // written.
            let mut seen = std::collections::BTreeSet::new();
            let _ = writeln!(out, "{pad}let rendered = match value.as_str() {{");
            for (canonical, wire) in pairs {
                if seen.insert(canonical.as_str()) {
                    let _ = writeln!(
                        out,
                        "{pad}    {canonical:?} => CompactString::from({wire:?}),"
                    );
                }
            }
            let _ = writeln!(out, "{pad}    _ => {{");
            encode_failure(out, node, codec, key, index_var, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}};");
        }
        (Pattern::Fraction { min, max }, MappingType::Decimal) => {
            let _ = writeln!(
                out,
                "{pad}let rendered = match codec::format_fraction(&value.to_string(), {min}, {max}) {{"
            );
            let _ = writeln!(out, "{pad}    Some(s) => s,");
            let _ = writeln!(out, "{pad}    None => {{");
            encode_failure(out, node, codec, key, index_var, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}};");
        }
        (Pattern::Latin1, _) => {
            let _ = writeln!(
                out,
                "{pad}let rendered = match codec::to_latin1(value.as_str()) {{"
            );
            let _ = writeln!(out, "{pad}    Some(s) => s,");
            let _ = writeln!(out, "{pad}    None => {{");
            encode_failure(out, node, codec, key, index_var, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}};");
        }
        (Pattern::Digits(n), _) => {
            let _ = writeln!(
                out,
                "{pad}let rendered = match codec::exact_digits(value.as_str(), {n}) {{"
            );
            let _ = writeln!(out, "{pad}    Some(s) => s,");
            let _ = writeln!(out, "{pad}    None => {{");
            encode_failure(out, node, codec, key, index_var, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}};");
        }
        _ => {
            let _ = writeln!(
                out,
                "{pad}let rendered = compile_error!(\"codec `{}` does not fit a `{}` node\");",
                codec.id, node.source_type
            );
        }
    }
}

/// Emits the `CODEC_INVALID` diagnostic for a canonical value the node's codec
/// cannot encode, and the empty rendering the writer's non-empty guard skips.
fn encode_failure(
    out: &mut String,
    node: &SourceNode,
    codec: &Codec,
    key: &str,
    index_var: Option<&str>,
    pad: &str,
) {
    let msg = format!(
        "format!(\"`{{value}}` cannot be encoded with codec `{}` ({})\")",
        codec.id, codec.lexical
    );
    DiagSpec::new("Severity::Error", "CODEC_INVALID", node.id.as_str(), &msg)
        .key(key)
        .path(&node.source_path)
        .index(index_var)
        .emit(out, pad);
    let _ = writeln!(out, "{pad}CompactString::new(\"\")");
}

/// Emits a writer-side missing-required diagnostic for a canonical field that
/// had no usable hub value.
fn emit_required_missing(
    out: &mut String,
    node: &SourceNode,
    key: &str,
    path: &str,
    index_var: Option<&str>,
    pad: &str,
) {
    DiagSpec::new(
        "Severity::Error",
        "REQUIRED_MISSING",
        node.id.as_str(),
        "\"required value is missing\"",
    )
    .key(key)
    .path(path)
    .index(index_var)
    .emit(out, pad);
}
