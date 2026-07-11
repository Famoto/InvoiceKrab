//! Writer generation: `write(mut main: MainKey) -> MappingResult<Root>`.
//!
//! The inverse of the reader. Constructs a `Default` source `Root` and assigns
//! each canonical field back to its primary `source_path`, rendering the typed
//! value to its source `String` form. Fallbacks and helper nodes are skipped.
//!
//! A node with a `constant` is written from that literal instead of the hub:
//! at root unconditionally, inside a collection only on non-empty items (so a
//! constant never resurrects an otherwise-empty element).
//!
//! A `clone_of` node fans its target key's hub value out to a second source
//! path — how a format stores one canonical value in several places (currency
//! attributes, duplicated VAT ids).
//!
//! The writer **consumes** the hub: canonical fields written exactly once move
//! their values into the target struct (`take`); a key written from more than
//! one node (a primary plus its clones) stays a borrow + clone.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::node::SourceNode;
use crate::source_model::SourceModelMeta;
use crate::types::MappingType;

use super::access::{assign_target_expr, collection_item_struct, walk_segments};
use super::diag::DiagSpec;
use super::naming::snake_case;
use super::plan::{Frame, GenCtx};

/// The canonical keys written more than once within one scope: a `clone_of`
/// node fans its target key out to a second source path, so the key is read
/// from the hub twice. These must stay borrow + clone reads of the hub; unique
/// keys move out. Nodes with a `constant` never read the hub, so they don't
/// count.
fn shared_hub_keys(
    scalars: &[&SourceNode],
    clones: &[&SourceNode],
    collections: &[&SourceNode],
) -> BTreeSet<String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut shared = BTreeSet::new();
    for node in scalars.iter().chain(clones).chain(collections) {
        if node.constant.is_some() {
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
/// key for a `clone_of` node.
fn hub_key(node: &SourceNode) -> &str {
    node.canonical_key
        .as_deref()
        .or(node.clone_of.as_deref())
        .expect("mapped or clone node")
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
    );
    for node in &ctx.plan.root_constants {
        out.push('\n');
        write_constant_block(out, ctx.source, node, &ctx.source.root, "source", 1);
    }
    for node in ctx
        .plan
        .root_scalars
        .iter()
        .filter(|n| n.constant.is_none())
        .chain(&ctx.plan.root_clones)
    {
        out.push('\n');
        write_scalar_block(
            out,
            ctx.source,
            node,
            &ctx.source.root,
            "main",
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

    out.push('\n');
    out.push_str("    MappingResult::new(Some(source), diagnostics)\n");
    out.push_str("}\n");
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
    let child_shared = shared_hub_keys(children, clones, nested);

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
    for child in children
        .iter()
        .filter(|n| n.constant.is_none())
        .chain(clones)
    {
        write_scalar_block(
            out,
            ctx.source,
            child,
            &src_item_struct,
            &hub_item,
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
    for constant in ctx.plan.constants_of(&coll.id) {
        write_constant_block(
            out,
            ctx.source,
            constant,
            &src_item_struct,
            &elem,
            indent + 2,
        );
    }
    let _ = writeln!(out, "{body}    {target}.push({elem});");
    let _ = writeln!(out, "{body}    {count} += 1;");
    let _ = writeln!(out, "{body}}}");
    let _ = writeln!(out, "{pad}}}");

    let min = coll.effective_min_items();
    if min > 0 {
        let _ = writeln!(out, "{pad}if {count} < {min} {{");
        let msg = format!(
            "format!(\"collection `{coll_key}` has {{{count}}} items, expected at least {min}\")"
        );
        DiagSpec::new("Severity::Error", "MIN_ITEMS", coll.id.as_str(), &msg)
            .key(coll_key)
            .path(&coll.source_path)
            .emit(out, &body);
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Emits the assignment of a node's `constant` literal to its source path. The
/// hub is never consulted; the literal was validated against the node's `type`
/// at compile time (E061), so it is emitted verbatim.
fn write_constant_block(
    out: &mut String,
    source: &SourceModelMeta,
    node: &SourceNode,
    start_struct: &str,
    src_var: &str,
    indent: usize,
) {
    let lit = node.constant.as_deref().expect("constant node");
    let path = &node.source_path;
    let target = assign_target_expr(source, start_struct, path, src_var);
    let optional = walk_segments(source, start_struct, path)
        .unwrap_or_default()
        .last()
        .is_some_and(|s| s.optional);
    let value = format!("CompactString::from({lit:?})");

    let pad = "    ".repeat(indent);
    let _ = writeln!(out, "{pad}// constant -> {path}");
    if optional {
        let _ = writeln!(out, "{pad}{target} = Some({value});");
    } else {
        let _ = writeln!(out, "{pad}{target} = {value};");
    }
}

/// Emits the write of one canonical field back to its source path, rendering the
/// typed value to the source `String` representation. When `take`, the hub value
/// is moved out instead of cloned (the field is written exactly once).
#[allow(clippy::too_many_arguments)]
fn write_scalar_block(
    out: &mut String,
    source: &SourceModelMeta,
    node: &SourceNode,
    start_struct: &str,
    hub_var: &str,
    src_var: &str,
    index_var: Option<&str>,
    indent: usize,
    take: bool,
) {
    let key = hub_key(node);
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
    let _ = writeln!(out, "{pad}    let rendered = {rendered};");
    let _ = writeln!(out, "{pad}    if !rendered.is_empty() {{");
    if repeated_leaf {
        let _ = writeln!(out, "{pad}        {target}.push(rendered);");
    } else if optional {
        let _ = writeln!(out, "{pad}        {target} = Some(rendered);");
    } else {
        let _ = writeln!(out, "{pad}        {target} = rendered;");
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
