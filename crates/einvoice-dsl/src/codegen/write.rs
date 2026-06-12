//! Writer generation: `write(main: &MainKey) -> MappingResult<Root>`.
//!
//! The inverse of the reader. Constructs a `Default` source `Root` and assigns
//! each canonical field back to its primary `source_path`, rendering the typed
//! value to its source `String` form. Fallbacks and helper nodes are skipped.

use std::fmt::Write as _;

use crate::node::SourceNode;
use crate::source_model::SourceModelMeta;
use crate::types::MappingType;

use super::access::{collection_item_struct, walk_segments};
use super::diag::DiagSpec;
use super::naming::field_name;
use super::plan::{Frame, GenCtx};

/// Emits the `write(main: &MainKey) -> MappingResult<Root>` function into `out`:
/// the inverse of the reader. It constructs a `Default` source `Root` and assigns
/// each canonical field back to its primary `source_path`. Fallbacks and helper
/// nodes are skipped.
pub(super) fn generate_write(out: &mut String, ctx: &GenCtx, root: &str) {
    out.push_str("/// Writes the canonical hub back into a typed source document.\n");
    let _ = writeln!(
        out,
        "pub fn write(main: &MainKey) -> MappingResult<{root}> {{"
    );
    out.push_str("    let mut diagnostics: Vec<MappingDiagnostic> = Vec::new();\n");
    let _ = writeln!(out, "    let mut source = {root}::default();");

    for node in &ctx.plan.root_scalars {
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
            },
        );
    }

    out.push('\n');
    out.push_str("    MappingResult::new(Some(source), diagnostics)\n");
    out.push_str("}\n");
}

/// Emits the write loop for one collection node: builds a source element per hub
/// item, writes the scalar children, recurses into nested collections, and pushes
/// the element. Mirrors `read_collection_block`.
fn write_collection_block(out: &mut String, ctx: &GenCtx, coll: &SourceNode, frame: &Frame) {
    let coll_key = coll.canonical_key.as_deref().expect("mapped collection");
    let src_item_struct =
        collection_item_struct(ctx.source, frame.parent_struct, &coll.source_path);
    let children = ctx.plan.children_of(&coll.id);
    let nested = ctx.plan.nested_collections_of(&coll.id);

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
    let _ = writeln!(
        out,
        "{pad}{parent_src}.{}.reserve({parent_hub}.{}.len());",
        coll.source_path,
        field_name(coll_key)
    );
    let _ = writeln!(
        out,
        "{pad}for ({idx}, {hub_item}) in {parent_hub}.{}.iter().enumerate() {{",
        field_name(coll_key)
    );
    let _ = writeln!(out, "{body}let mut {elem} = {src_item_struct}::default();");
    for child in children {
        write_scalar_block(
            out,
            ctx.source,
            child,
            &src_item_struct,
            &hub_item,
            &elem,
            Some(&idx),
            indent + 1,
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
            },
        );
    }
    let _ = writeln!(out, "{body}if !{elem}.is_empty() {{");
    let _ = writeln!(
        out,
        "{body}    {parent_src}.{}.push({elem});",
        coll.source_path
    );
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

/// Emits the write of one canonical field back to its source path, rendering the
/// typed value to the source `String` representation.
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
) {
    let key = node.canonical_key.as_deref().expect("mapped scalar");
    let path = &node.source_path;
    let field = field_name(key);
    let segs = walk_segments(source, start_struct, path).unwrap_or_default();
    let optional = segs.iter().any(|s| s.optional);
    // A `multiple` node's source field is `Vec<String>`: the writer pushes the
    // single canonical value as one element (a joined value stays joined).
    let repeated_leaf = segs
        .last()
        .is_some_and(|s| s.repeated && s.struct_name.is_none());
    // Render the typed value to a source String.
    let rendered = match node.source_type {
        MappingType::Decimal | MappingType::Boolean => "value.to_string()",
        _ => "value.clone()",
    };

    let pad = "    ".repeat(indent);
    let _ = writeln!(out, "{pad}// {key} -> {path}");
    let _ = writeln!(out, "{pad}if let Some(value) = &{hub_var}.{field} {{");
    let _ = writeln!(out, "{pad}    let rendered = {rendered};");
    let _ = writeln!(out, "{pad}    if !rendered.is_empty() {{");
    if repeated_leaf {
        let _ = writeln!(out, "{pad}        {src_var}.{path}.push(rendered);");
    } else if optional {
        let _ = writeln!(out, "{pad}        {src_var}.{path} = Some(rendered);");
    } else {
        let _ = writeln!(out, "{pad}        {src_var}.{path} = rendered;");
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
