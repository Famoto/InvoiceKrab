//! Reader generation: `read(source: &Root) -> MappingResult<MainKey>`.
//!
//! Per node: reads the source field, applies the normalize chain, falls back
//! through `fallbacks`, decodes/validates by `type`, applies an optional
//! `adapter`, enforces `required`/`min_items`, and assigns into the typed hub.

use std::fmt::Write as _;

use crate::multiple::MultiplePolicy;
use crate::node::SourceNode;
use crate::normalize::NormalizeOp;
use crate::source_model::SourceModelMeta;
use crate::types::MappingType;

use super::access::{access_expr, collection_item_struct, normalize_chain};
use super::diag::DiagSpec;
use super::naming::{field_name, item_struct_name};
use super::plan::{Frame, GenCtx};

/// The target a read block assigns into: the struct variable (`main`/`item`),
/// the source access base (`source`/`element`), and the index var for collection
/// diagnostics.
struct Target<'a> {
    /// The hub struct variable assigned into (`main` or `item`).
    struct_var: &'a str,
    /// The source access base (`source` at root, `element` in a collection).
    base_var: &'a str,
    /// The collection index variable for diagnostics, if inside a collection.
    index_var: Option<&'a str>,
}

/// Emits the `read(source: &Root) -> MappingResult<MainKey>` function into `out`.
pub(super) fn generate_read(out: &mut String, ctx: &GenCtx, root: &str) {
    out.push_str("/// Reads the typed source document into the canonical hub.\n");
    let _ = writeln!(
        out,
        "pub fn read(source: &{root}) -> MappingResult<MainKey> {{"
    );
    out.push_str("    let mut diagnostics: Vec<MappingDiagnostic> = Vec::new();\n");
    out.push_str("    let mut main = MainKey::default();\n");

    let root_target = Target {
        struct_var: "main",
        base_var: "source",
        index_var: None,
    };
    for node in &ctx.plan.root_scalars {
        out.push('\n');
        read_scalar_block(out, ctx, node, &ctx.source.root, 1, &root_target);
    }

    for coll in &ctx.plan.root_collections {
        out.push('\n');
        read_collection_block(
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
    out.push_str("    MappingResult::new(Some(main), diagnostics)\n");
    out.push_str("}\n");
}

/// Emits the read block for one scalar node: read + normalize + fallbacks +
/// decode + assign into `target.struct_var.<field>`.
fn read_scalar_block(
    out: &mut String,
    ctx: &GenCtx,
    node: &SourceNode,
    start_struct: &str,
    indent: usize,
    target: &Target,
) {
    let pad = "    ".repeat(indent);
    let key = node
        .canonical_key
        .as_deref()
        .expect("scalar read block requires a canonical key");

    if let Some(policy) = node.multiple {
        let _ = writeln!(out, "{pad}// {} -> {key} (multiple)", node.id);
        read_multi_values(out, node, key, policy, indent, target);
    } else {
        let _ = writeln!(out, "{pad}// {} -> {key}", node.id);
        let binding = if node.fallbacks.is_empty() {
            "value"
        } else {
            "mut value"
        };
        let _ = writeln!(
            out,
            "{pad}let {binding}: Option<String> = {};",
            read_one_value(
                ctx.source,
                start_struct,
                &node.source_path,
                &node.normalize,
                target
            )
        );

        // Fallbacks, in declared order. The fallback chain is the same for every
        // fallback of this node, so build the literal once.
        let chain_lit = if node.fallbacks.is_empty() {
            String::new()
        } else {
            format!("vec![{}]", fallback_chain_literal(node))
        };
        for fb_id in &node.fallbacks {
            let Some(fb) = ctx.ir.nodes.get(fb_id) else {
                continue;
            };
            let _ = writeln!(out, "{pad}if value.is_none() {{");
            let _ = writeln!(
                out,
                "{pad}    value = {};",
                read_one_value(
                    ctx.source,
                    start_struct,
                    &fb.source_path,
                    &fb.normalize,
                    target
                )
            );
            let _ = writeln!(out, "{pad}    if value.is_some() {{");
            let msg = format!("format!(\"used fallback `{fb_id}` for `{key}`\")");
            DiagSpec::new("Severity::Info", "FALLBACK_USED", node.id.as_str(), &msg)
                .key(key)
                .path(&fb.source_path)
                .index(target.index_var)
                .chain(&chain_lit)
                .emit(out, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}}");
        }
    }

    // Decode + assign, or a required-missing diagnostic.
    let _ = writeln!(out, "{pad}if let Some(raw) = value {{");
    decode_and_assign(out, node, key, indent + 1, target);
    let _ = writeln!(out, "{pad}}} else if {} {{", node.required);
    DiagSpec::new(
        "Severity::Error",
        "REQUIRED_MISSING",
        node.id.as_str(),
        "\"required value is missing\"",
    )
    .key(key)
    .path(&node.source_path)
    .index(target.index_var)
    .emit(out, &format!("{pad}    "));
    let _ = writeln!(out, "{pad}}}");
}

/// Emits the multi-value read for a `multiple` node: collect the normalized
/// `Vec<String>` from the repeated source field, then collapse it to
/// `value: Option<String>` per the policy (`error` / `first` / `join`),
/// emitting a `MULTIPLE_VALUES` diagnostic where the policy calls for one.
fn read_multi_values(
    out: &mut String,
    node: &SourceNode,
    key: &str,
    policy: MultiplePolicy,
    indent: usize,
    target: &Target,
) {
    let pad = "    ".repeat(indent);
    // The repeated leaf is a plain `Vec<String>` field (interior segments are
    // never Option/Vec), so the source expression is a direct field access.
    // Each item runs through the node's normalize chain; `empty_as_missing`
    // drops the item.
    let item_chain = normalize_chain("Some(s.as_str())", &node.normalize);
    let _ = writeln!(
        out,
        "{pad}let values: Vec<String> = {}.{}.iter().filter_map(|s| {item_chain}).collect();",
        target.base_var, node.source_path
    );

    match policy {
        MultiplePolicy::Error => {
            let _ = writeln!(out, "{pad}if values.len() > 1 {{");
            let msg = format!(
                "format!(\"found {{}} values for single-valued `{key}`\", values.len())"
            );
            DiagSpec::new("Severity::Error", "MULTIPLE_VALUES", node.id.as_str(), &msg)
                .key(key)
                .path(&node.source_path)
                .index(target.index_var)
                .emit(out, &format!("{pad}    "));
            let _ = writeln!(out, "{pad}}}");
            let _ = writeln!(
                out,
                "{pad}let value: Option<String> = if values.len() == 1 {{ values.into_iter().next() }} else {{ None }};"
            );
        }
        MultiplePolicy::First => {
            let _ = writeln!(out, "{pad}if values.len() > 1 {{");
            let msg =
                format!("format!(\"using the first of {{}} values for `{key}`\", values.len())");
            DiagSpec::new(
                "Severity::Warning",
                "MULTIPLE_VALUES",
                node.id.as_str(),
                &msg,
            )
            .key(key)
            .path(&node.source_path)
            .index(target.index_var)
            .emit(out, &format!("{pad}    "));
            let _ = writeln!(out, "{pad}}}");
            let _ = writeln!(
                out,
                "{pad}let value: Option<String> = values.into_iter().next();"
            );
        }
        MultiplePolicy::Join => {
            let sep = node
                .join_with
                .as_deref()
                .expect("E040 guarantees join_with for multiple = join");
            let _ = writeln!(
                out,
                "{pad}let value: Option<String> = if values.is_empty() {{ None }} else {{ Some(values.join({sep:?})) }};"
            );
        }
    }
}

/// The `vec![...]` body of node-id string literals naming the fallback chain
/// (the primary node first, then each fallback in order).
fn fallback_chain_literal(node: &SourceNode) -> String {
    let mut parts = vec![format!("{:?}.to_string()", node.id.as_str())];
    for fb in &node.fallbacks {
        parts.push(format!("{:?}.to_string()", fb.as_str()));
    }
    parts.join(", ")
}

/// Emits the read loop for one collection node and its children, building an item
/// struct per source element and pushing it into the hub collection field.
/// Recurses into nested collections; `frame` carries the depth (for unique loop
/// variables), indent, and enclosing hub/source/struct context.
fn read_collection_block(out: &mut String, ctx: &GenCtx, coll: &SourceNode, frame: &Frame) {
    let coll_key = coll
        .canonical_key
        .as_deref()
        .expect("collection read block requires a canonical key");
    let src_item_struct =
        collection_item_struct(ctx.source, frame.parent_struct, &coll.source_path);
    let hub_item = item_struct_name(coll_key);
    let hub_field = field_name(coll_key);
    let children = ctx.plan.children_of(&coll.id);
    let nested = ctx.plan.nested_collections_of(&coll.id);

    // Per-depth variable names keep nested loops from shadowing each other.
    let depth = frame.depth;
    let indent = frame.indent;
    let parent_hub = frame.parent_hub;
    let parent_src = frame.parent_src;
    let elem = format!("element{depth}");
    let item = format!("item{depth}");
    let idx = format!("idx{depth}");
    let count = format!("count{depth}");
    let pad = "    ".repeat(indent);
    let body = "    ".repeat(indent + 1);

    let _ = writeln!(out, "{pad}// {} -> {coll_key} (collection)", coll.id);
    let _ = writeln!(
        out,
        "{pad}let {count} = {parent_src}.{}.len();",
        coll.source_path
    );
    // The element count is known before the loop, so reserve the hub collection
    // once instead of letting it regrow as items are pushed.
    let _ = writeln!(out, "{pad}{parent_hub}.{hub_field}.reserve({count});");
    let _ = writeln!(
        out,
        "{pad}for ({idx}, {elem}) in {parent_src}.{}.iter().enumerate() {{",
        coll.source_path
    );
    let _ = writeln!(out, "{body}let mut {item} = {hub_item}::default();");
    let target = Target {
        struct_var: &item,
        base_var: &elem,
        index_var: Some(&idx),
    };
    for child in children {
        out.push('\n');
        read_scalar_block(out, ctx, child, &src_item_struct, indent + 1, &target);
    }
    for nested_coll in nested {
        out.push('\n');
        read_collection_block(
            out,
            ctx,
            nested_coll,
            &Frame {
                depth: depth + 1,
                indent: indent + 1,
                parent_hub: &item,
                parent_src: &elem,
                parent_struct: &src_item_struct,
            },
        );
    }
    out.push('\n');
    let _ = writeln!(out, "{body}{parent_hub}.{hub_field}.push({item});");
    let _ = writeln!(out, "{pad}}}");

    // min_items / required underflow.
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

/// Builds the `Option<String>` expression that reads one source field and applies
/// its normalize chain.
fn read_one_value(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    normalize: &[NormalizeOp],
    target: &Target,
) -> String {
    let access = access_expr(source, start_struct, path, target.base_var);
    normalize_chain(&access, normalize)
}

/// Emits the decode + adapter + assign snippet, given `raw: String` is in scope
/// inside `if let Some(raw) = value`.
fn decode_and_assign(
    out: &mut String,
    node: &SourceNode,
    key: &str,
    indent: usize,
    target: &Target,
) {
    let pad = "    ".repeat(indent);
    let field = field_name(key);
    let lhs = format!("{}.{field}", target.struct_var);

    // Optional adapter: transform the raw string first (String -> String).
    let (decode_pad, has_adapter_wrap) = if let Some(adapter) = &node.adapter {
        let _ = writeln!(out, "{pad}let adapted = match adapter::{adapter}(&raw) {{");
        let _ = writeln!(out, "{pad}    Ok(s) => Some(s),");
        let _ = writeln!(out, "{pad}    Err(err) => {{");
        DiagSpec::new(
            "Severity::Error",
            "ADAPTER_FAILED",
            node.id.as_str(),
            "err.to_string()",
        )
        .key(key)
        .index(target.index_var)
        .emit(out, &format!("{pad}        "));
        let _ = writeln!(out, "{pad}        None");
        let _ = writeln!(out, "{pad}    }}");
        let _ = writeln!(out, "{pad}}};");
        let _ = writeln!(out, "{pad}if let Some(raw) = adapted {{");
        ("    ".repeat(indent + 1), true)
    } else {
        (pad.clone(), false)
    };

    decode_body(out, node, key, &decode_pad, &lhs, target);

    if has_adapter_wrap {
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Emits the type-specific decode of `raw: String` into the typed `lhs` field.
fn decode_body(
    out: &mut String,
    node: &SourceNode,
    key: &str,
    pad: &str,
    lhs: &str,
    target: &Target,
) {
    match node.source_type {
        MappingType::Decimal => {
            let _ = writeln!(out, "{pad}match Decimal::from_str(raw.trim()) {{");
            let _ = writeln!(out, "{pad}    Ok(d) => {lhs} = Some(d),");
            let _ = writeln!(out, "{pad}    Err(_) => {{");
            DiagSpec::new(
                "Severity::Error",
                "TYPE_INVALID",
                node.id.as_str(),
                "format!(\"`{raw}` is not a valid decimal\")",
            )
            .key(key)
            .index(target.index_var)
            .emit(out, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}}");
        }
        MappingType::Boolean => {
            let _ = writeln!(out, "{pad}match raw.trim().parse::<bool>() {{");
            let _ = writeln!(out, "{pad}    Ok(b) => {lhs} = Some(b),");
            let _ = writeln!(out, "{pad}    Err(_) => {{");
            DiagSpec::new(
                "Severity::Error",
                "TYPE_INVALID",
                node.id.as_str(),
                "format!(\"`{raw}` is not a valid boolean\")",
            )
            .key(key)
            .index(target.index_var)
            .emit(out, &format!("{pad}        "));
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}}");
        }
        ty if lexical_validator(ty).is_some() => {
            let (validator, human) = lexical_validator(ty).expect("guarded by match arm");
            let _ = writeln!(out, "{pad}if validate::{validator}(raw.trim()) {{");
            let _ = writeln!(out, "{pad}    {lhs} = Some(raw);");
            let _ = writeln!(out, "{pad}}} else {{");
            let msg = format!("format!(\"`{{raw}}` is not a valid {human}\")");
            DiagSpec::new("Severity::Error", "TYPE_INVALID", node.id.as_str(), &msg)
                .key(key)
                .index(target.index_var)
                .emit(out, &format!("{pad}    "));
            let _ = writeln!(out, "{pad}}}");
        }
        // Free text: assigned verbatim.
        _ => {
            let _ = writeln!(out, "{pad}{lhs} = Some(raw);");
        }
    }
}

/// The runtime `validate` predicate and human label for a type that has a lexical
/// form, or `None` for free-text types (`string`/`identifier`).
fn lexical_validator(ty: MappingType) -> Option<(&'static str, &'static str)> {
    match ty {
        MappingType::Currency => Some(("is_currency", "currency code")),
        MappingType::Date => Some(("is_date", "date")),
        MappingType::Datetime => Some(("is_datetime", "date-time")),
        MappingType::UnitCode => Some(("is_unit_code", "unit code")),
        _ => None,
    }
}
