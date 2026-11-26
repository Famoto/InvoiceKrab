//! Reader generation: `read(mut source: Root) -> MappingResult<MainKey>`.
//!
//! Per node: reads the source field, applies the normalize chain, falls back
//! through `fallbacks`, decodes/validates by `type` (through the node's `codec`
//! when it has one: the lexical form becomes the canonical form, a mismatch is
//! `CODEC_INVALID`, and a wire attribute that disagrees with the codec's is
//! `CODEC_WIRE_MISMATCH`), enforces `required`, and assigns into the typed hub.
//! A required collection with no items is `REQUIRED_MISSING` like a scalar.
//!
//! A `clone_of` node never fills the hub: after every primary assign in its
//! scope, its path is read and decoded only to check the copy against the
//! canonical value (`CLONE_MISMATCH` warning on disagreement). A `$parent.Key`
//! / `$root.Key` clone compares against the enclosing scope's / the root's
//! value, which is final by then (outer scalars are assigned before the loops).
//!
//! A lifted key (`canonical_key = "$root.Key"` inside a collection) is decoded
//! per occurrence and collapsed into the enclosing scope's field: the first
//! value fills it, a later one that differs is `SCOPE_DISAGREE` (an error, or
//! a warning that keeps the first under `multiple = "first"`).
//!
//! When the mapping aliases physical elements (`match` selectors), a logical
//! field's nodes read the items of the physical element its selector picks
//! (the first such item for a single-valued one); `demux` first warns
//! `MATCH_MULTIPLE` for each single-valued logical node that matched several.
//!
//! The reader borrows the source record: each value is read by its id and
//! copied into the hub (inline, without a heap allocation, for values of up
//! to 24 bytes), so a path can be read any number of times.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::codec::{Codec, Pattern};
use crate::multiple::MultiplePolicy;
use crate::node::{DerivationScope, Lift, SourceNode};
use crate::normalize::NormalizeOp;
use crate::source_model::{SourceModelMeta, xml_field_name};
use crate::types::MappingType;

use super::access::{
    access_expr, collection_item_struct, items_expr, normalize_chain, values_expr,
};
use super::diag::DiagSpec;
use super::naming::{item_struct_name, snake_case};
use super::plan::{Frame, GenCtx};

/// The target a read block assigns into: the struct variable (`main`/`item`),
/// the source access base (`source`/`element`), and the index var for collection
/// diagnostics.
struct Target<'a> {
    /// The hub struct variable assigned into (`main` or `item`).
    struct_var: &'a str,
    /// The enclosing scope's hub variable (`None` at root), which a
    /// `$parent.Key` lifted key collapses into.
    parent_var: Option<&'a str>,
    /// The source access base (`source` at root, `element` in a collection).
    base_var: &'a str,
    /// The collection index variable for diagnostics, if inside a collection.
    index_var: Option<&'a str>,
    /// The scope's alternative sets ([`crate::alt::groups`]): their nodes
    /// decode into `alt_<position>` locals, combined after the scope's scalars.
    alts: &'a [Vec<&'a SourceNode>],
}

/// Emits the `read(source: Root) -> MappingResult<MainKey>` function into
/// `out`. The record is indexed once and read in place; nothing is moved out.
pub(super) fn generate_read(out: &mut String, ctx: &GenCtx, root: &str) {
    out.push_str("/// Reads the source document's record into the canonical hub.\n");
    let _ = writeln!(
        out,
        "pub fn read(source: {root}) -> MappingResult<MainKey> {{"
    );
    out.push_str("    let mut diagnostics: Vec<MappingDiagnostic> = Vec::new();\n");
    out.push_str("    let mut main = MainKey::default();\n");
    let _ = writeln!(
        out,
        "    let document = source;\n    let source = Index::new(document.0.rec(), {root}::SIZE);"
    );
    if super::source::root_has_alias_io(ctx.source) {
        // Aliased elements: the logical fields the mapping reads pick their
        // items from the physical element by selector; a single-valued
        // logical node that matches more than one item warns first.
        // `$root.Key` references are read before anything is picked.
        let (refs, refs_arg) = super::source::match_refs_binding(ctx.source, "source");
        out.push_str(&refs);
        let root = &ctx.source.root;
        if refs_arg.is_empty() {
            let _ = writeln!(
                out,
                "    {root}::demux(source.rec(), 0, &mut |node, extra| {{"
            );
        } else {
            let _ = writeln!(
                out,
                "    {root}::demux(source.rec(), 0, {refs_arg}, &mut |node, extra| {{"
            );
        }
        out.push_str("        diagnostics.push(MappingDiagnostic::new(\n");
        out.push_str("            Severity::Warning,\n");
        out.push_str("            \"MATCH_MULTIPLE\",\n");
        out.push_str("            node,\n");
        out.push_str("            format!(\"{extra} more item(s) match the node's selector; only the first is read\"),\n");
        out.push_str("        ));\n");
        out.push_str("    });\n");
    }

    let root_alts = crate::alt::groups(ctx.plan.root_scalars.iter().copied());
    let root_target = Target {
        struct_var: "main",
        parent_var: None,
        base_var: "source",
        index_var: None,
        alts: &root_alts,
    };
    for node in &ctx.plan.root_scalars {
        out.push('\n');
        read_scalar_block(out, ctx, node, &ctx.source.root, 1, &root_target);
    }
    combine_alternatives(out, 1, &root_target);

    // Clone checks run after every primary assign, so the canonical value they
    // compare against is final.
    for clone in &ctx.plan.root_clones {
        out.push('\n');
        read_clone_check_block(out, ctx, clone, &ctx.source.root, 1, &root_target, None);
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
                owned: false,
            },
        );
    }

    out.push('\n');
    out.push_str("    MappingResult::new(Some(main), diagnostics)\n");
    out.push_str("}\n");
}

/// Emits the read block for one scalar node: read + normalize + fallbacks +
/// decode + assign into `target.struct_var.<field>`.
#[allow(clippy::too_many_arguments)]
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
        read_multi_values(
            out,
            ctx.source,
            start_struct,
            node,
            key,
            policy,
            indent,
            target,
        );
    } else {
        let _ = writeln!(out, "{pad}// {} -> {key}", node.id);
        if let Some(codec) = ctx.translating_codec_of(node) {
            read_wire_checks(out, ctx, node, codec, key, start_struct, indent, target);
        }
        let binding = if node.fallbacks.is_empty() {
            "value"
        } else {
            "mut value"
        };
        let _ = writeln!(
            out,
            "{pad}let {binding}: Option<CompactString> = {};",
            node_value(ctx, node, start_struct, target)
        );

        // Fallbacks, in declared order. The fallback chain is the same for every
        // fallback of this node, so build the literal once.
        let chain_lit = if node.fallbacks.is_empty() {
            String::new()
        } else {
            format!("vec![{}]", fallback_chain_literal(node))
        };
        // A placeholder stands for "nothing here": filtered before the
        // fallbacks (so they are tried) and after them.
        let placeholder_filter = node
            .placeholder
            .as_ref()
            .and_then(|p| p.values())
            .map(|values| {
                let values: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
                format!(
                    "value = value.filter(|v| !matches!(v.trim(), {}));",
                    values.join(" | ")
                )
            });
        if let Some(filter) = placeholder_filter
            .as_ref()
            .filter(|_| !node.fallbacks.is_empty())
        {
            let _ = writeln!(out, "{pad}{filter}");
        }
        for fb_id in &node.fallbacks {
            let Some(fb) = ctx.ir.nodes.get(fb_id) else {
                continue;
            };
            let _ = writeln!(out, "{pad}if value.is_none() {{");
            let _ = writeln!(
                out,
                "{pad}    value = {};",
                node_value(ctx, fb, start_struct, target)
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
        if let Some(filter) = &placeholder_filter {
            let _ = writeln!(out, "{pad}let {filter}");
        }
    }

    // Decode + assign; only a required field gets the missing-value branch,
    // so optional fields emit no dead diagnostic code. A `default` fills the
    // key instead, so a defaulted field is never missing. A lifted key decodes
    // into a local first and is collapsed into its scope's field below.
    let lhs = if node.lift.is_some() {
        let _ = writeln!(out, "{pad}let mut lifted = None;");
        "lifted".to_string()
    } else if target.alts.iter().flatten().any(|n| n.id == node.id) {
        let _ = writeln!(out, "{pad}let mut alt_{} = None;", node.position);
        format!("alt_{}", node.position)
    } else {
        format!("{}.{}", target.struct_var, snake_case(key))
    };
    let _ = writeln!(out, "{pad}if let Some(raw) = value {{");
    read_length_check(out, ctx, node, key, target, &format!("{pad}    "));
    decode_body(
        out,
        node,
        ctx.translating_codec_of(node),
        key,
        &"    ".repeat(indent + 1),
        &lhs,
        target,
    );
    if let Some(default) = &node.default {
        let _ = writeln!(out, "{pad}}} else {{");
        let _ = writeln!(
            out,
            "{pad}    {lhs} = Some({});",
            typed_literal(node.source_type, default)
        );
    } else if node.required {
        let _ = writeln!(out, "{pad}}} else {{");
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
    }
    let _ = writeln!(out, "{pad}}}");
    if let Some(lift) = node.lift {
        collapse_lifted(out, node, key, lift, indent, target);
    }
}

/// Emits, for each alternative set of the scope, the hand-over of the first
/// `alt_<position>` with a value (file order) into the hub field, with the
/// supplier's `when` values filled in (a list's first); a later alternative that
/// also has a value is `MULTIPLE_SOURCES`.
fn combine_alternatives(out: &mut String, indent: usize, target: &Target) {
    let pad = "    ".repeat(indent);
    for group in target.alts {
        let key = group[0].canonical_key.as_deref().expect("keyed");
        let field = format!("{}.{}", target.struct_var, snake_case(key));
        let _ = writeln!(
            out,
            "\n{pad}// alternatives of {key}: the first with a value supplies it"
        );
        for (i, node) in group.iter().enumerate() {
            let _ = writeln!(out, "{pad}if let Some(found) = alt_{} {{", node.position);
            let _ = writeln!(out, "{pad}    if {field}.is_none() {{");
            let _ = writeln!(out, "{pad}        {field} = Some(found);");
            for (when_key, cond) in node.when.iter().flatten() {
                // A value test is filled in (a list: its first value); a
                // presence test says nothing to fill.
                let Some(values) = cond.values().filter(|vs| !vs.is_empty()) else {
                    continue;
                };
                let when_field = format!("{}.{}", target.struct_var, snake_case(when_key));
                let _ = writeln!(out, "{pad}        if {when_field}.is_none() {{");
                let _ = writeln!(
                    out,
                    "{pad}            {when_field} = Some(CompactString::from({:?}));",
                    values[0]
                );
                let accepted: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
                let _ = writeln!(
                    out,
                    "{pad}        }} else if !matches!({when_field}.as_deref(), Some({})) {{",
                    accepted.join(" | ")
                );
                let msg = format!(
                    "format!(\"the value comes from `{}`, written when `{when_key}` is {}, but the document says `{{}}`\", {when_field}.as_deref().unwrap_or_default())",
                    node.id,
                    cond.display()
                );
                DiagSpec::new(
                    "Severity::Warning",
                    "VALUE_INCONSISTENT",
                    node.id.as_str(),
                    &msg,
                )
                .key(when_key)
                .path(&node.source_path)
                .index(target.index_var)
                .emit(out, &format!("{pad}            "));
                let _ = writeln!(out, "{pad}        }}");
            }
            if i > 0 {
                let _ = writeln!(out, "{pad}    }} else {{");
                let msg = format!(
                    "\"`{key}` is also given here; the value of an earlier alternative is used\""
                );
                DiagSpec::new(
                    "Severity::Warning",
                    "MULTIPLE_SOURCES",
                    node.id.as_str(),
                    &msg,
                )
                .key(key)
                .path(&node.source_path)
                .index(target.index_var)
                .emit(out, &format!("{pad}        "));
            }
            let _ = writeln!(out, "{pad}    }}");
            let _ = writeln!(out, "{pad}}}");
        }
    }
}

/// Emits the collapse of one occurrence's `lifted` value into the field of the
/// scope the key lives in: the first value fills it, a later different one is
/// `SCOPE_DISAGREE`.
fn collapse_lifted(
    out: &mut String,
    node: &SourceNode,
    key: &str,
    lift: Lift,
    indent: usize,
    target: &Target,
) {
    let pad = "    ".repeat(indent);
    let scope_var = match lift.scope {
        DerivationScope::Root => "main",
        _ => target
            .parent_var
            .expect("E136 rejects a lift outside a collection before codegen"),
    };
    let field = format!("{scope_var}.{}", snake_case(key));
    let (severity, consequence) = if lift.keep_first {
        ("Severity::Warning", "the first is kept")
    } else {
        ("Severity::Error", "occurrences of one value must agree")
    };
    let _ = writeln!(out, "{pad}if let Some(found) = lifted {{");
    let _ = writeln!(out, "{pad}    if let Some(first) = &{field} {{");
    let _ = writeln!(out, "{pad}        if *first != found {{");
    let msg = format!(
        "format!(\"`{{found}}` differs from `{{first}}` read from an earlier occurrence; {consequence}\")"
    );
    DiagSpec::new(severity, "SCOPE_DISAGREE", node.id.as_str(), &msg)
        .key(key)
        .path(&node.source_path)
        .index(target.index_var)
        .emit(out, &format!("{pad}            "));
    let _ = writeln!(out, "{pad}        }}");
    let _ = writeln!(out, "{pad}    }} else {{");
    let _ = writeln!(out, "{pad}        {field} = Some(found);");
    let _ = writeln!(out, "{pad}    }}");
    let _ = writeln!(out, "{pad}}}");
}

/// Emits, for each wire attribute of the node's codec, a warning when the
/// document carries the attribute with a different value than the codec
/// writes (`<DateTimeString format="610">` under a `102` codec). The attribute
/// lives on the node's element struct, next to the `$text` value the node's
/// path ends in.
#[allow(clippy::too_many_arguments)]
fn read_wire_checks(
    out: &mut String,
    ctx: &GenCtx,
    node: &SourceNode,
    codec: &Codec,
    key: &str,
    start_struct: &str,
    indent: usize,
    target: &Target,
) {
    let Some(element_path) = node.source_path.strip_suffix(".value") else {
        return;
    };
    let pad = "    ".repeat(indent);
    for (attr, expected) in &codec.wire {
        let attr_path = format!("{element_path}.{}", snake_case(attr));
        let access = access_expr(ctx.source, start_struct, &attr_path, target.base_var);
        let _ = writeln!(out, "{pad}if let Some(found) = {access} {{");
        let _ = writeln!(out, "{pad}    if found != {expected:?} {{");
        let msg = format!(
            "format!(\"attribute `{attr}` is `{{found}}`, codec `{}` writes `{expected}`\")",
            codec.id
        );
        DiagSpec::new(
            "Severity::Warning",
            "CODEC_WIRE_MISMATCH",
            node.id.as_str(),
            &msg,
        )
        .key(key)
        .path(&attr_path)
        .index(target.index_var)
        .emit(out, &format!("{pad}        "));
        let _ = writeln!(out, "{pad}    }}");
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Emits the consistency check for one `clone_of` node: read + normalize the
/// copy's path, decode it under the target key's type, and warn
/// (`CLONE_MISMATCH`) when the decoded copy disagrees with the canonical value
/// already assigned by the primary node. The copy never fills the hub; an
/// absent copy is fine (the writer will emit it on the way out). `parent_hub`
/// is the enclosing scope's hub variable (`None` at root) for `$parent` clones.
#[allow(clippy::too_many_arguments)]
fn read_clone_check_block(
    out: &mut String,
    ctx: &GenCtx,
    node: &SourceNode,
    start_struct: &str,
    indent: usize,
    target: &Target,
    parent_hub: Option<&str>,
) {
    let pad = "    ".repeat(indent);
    let body = "    ".repeat(indent + 1);
    let derivation = node
        .derivation()
        .expect("clone node")
        .expect("E093 rejects malformed derivations before codegen");
    let key = derivation.key;
    let hub_var = match derivation.scope {
        DerivationScope::Own => target.struct_var,
        DerivationScope::Root => "main",
        DerivationScope::Parent => {
            parent_hub.expect("E093 rejects `$parent` at root scope before codegen")
        }
    };
    let canonical = format!("{hub_var}.{}", snake_case(key));

    let _ = writeln!(out, "{pad}// {}: copy of {key}, consistency check", node.id);
    let _ = writeln!(out, "{pad}{{");
    let _ = writeln!(
        out,
        "{body}let value: Option<CompactString> = {};",
        node_value(ctx, node, start_struct, target)
    );
    let _ = writeln!(out, "{body}if let Some(raw) = value {{");
    read_length_check(out, ctx, node, key, target, &format!("{body}    "));
    let _ = writeln!(out, "{body}    let mut clone_value = None;");
    decode_body(
        out,
        node,
        ctx.translating_codec_of(node),
        key,
        &"    ".repeat(indent + 2),
        "clone_value",
        target,
    );
    let _ = writeln!(out, "{body}    if let Some(found) = clone_value {{");
    let _ = writeln!(
        out,
        "{body}        if {canonical}.as_ref() != Some(&found) {{"
    );
    let msg =
        format!("format!(\"copy value `{{found}}` does not match the canonical `{key}` value\")");
    DiagSpec::new(
        "Severity::Warning",
        "CLONE_MISMATCH",
        node.id.as_str(),
        &msg,
    )
    .key(key)
    .path(&node.source_path)
    .index(target.index_var)
    .emit(out, &format!("{body}            "));
    let _ = writeln!(out, "{body}        }}");
    let _ = writeln!(out, "{body}    }}");
    let _ = writeln!(out, "{body}}}");
    let _ = writeln!(out, "{pad}}}");
}

/// Emits, inside `if let Some(raw) = value`, the `VALUE_TOO_LONG` warning for
/// a `raw` longer than the node's `max_length` (the source is assumed valid,
/// so an over-long value only warns).
fn read_length_check(
    out: &mut String,
    ctx: &GenCtx,
    node: &SourceNode,
    key: &str,
    target: &Target,
    pad: &str,
) {
    let Some(limit) = ctx.max_length_of(node) else {
        return;
    };
    let _ = writeln!(out, "{pad}if raw.chars().count() > {limit} {{");
    let msg = format!(
        "format!(\"value has {{}} characters, more than the {limit} the target allows\", raw.chars().count())"
    );
    DiagSpec::new(
        "Severity::Warning",
        "VALUE_TOO_LONG",
        node.id.as_str(),
        &msg,
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
#[allow(clippy::too_many_arguments)]
fn read_multi_values(
    out: &mut String,
    source: &SourceModelMeta,
    start_struct: &str,
    node: &SourceNode,
    key: &str,
    policy: MultiplePolicy,
    indent: usize,
    target: &Target,
) {
    let pad = "    ".repeat(indent);
    // The repeated scalar's values, each through the node's normalize chain;
    // `empty_as_missing` drops the value.
    let values = values_expr(source, start_struct, &node.source_path, target.base_var);
    let item_chain = normalize_chain("Some(s)", &node.normalize, false);
    let _ = writeln!(
        out,
        "{pad}let values: Vec<CompactString> = {values}.filter_map(|s| {item_chain}).collect();",
    );

    match policy {
        MultiplePolicy::Error => {
            let _ = writeln!(out, "{pad}if values.len() > 1 {{");
            let msg =
                format!("format!(\"found {{}} values for single-valued `{key}`\", values.len())");
            DiagSpec::new("Severity::Error", "MULTIPLE_VALUES", node.id.as_str(), &msg)
                .key(key)
                .path(&node.source_path)
                .index(target.index_var)
                .emit(out, &format!("{pad}    "));
            let _ = writeln!(out, "{pad}}}");
            let _ = writeln!(
                out,
                "{pad}let value: Option<CompactString> = if values.len() == 1 {{ values.into_iter().next() }} else {{ None }};"
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
                "{pad}let value: Option<CompactString> = values.into_iter().next();"
            );
        }
        MultiplePolicy::Join => {
            let sep = node
                .join_with
                .as_deref()
                .expect("E040 guarantees join_with for multiple = join");
            // Slice join yields a `String`; `.into()` moves it into the inline
            // string type (O(1) when the joined value is heap-sized).
            let _ = writeln!(
                out,
                "{pad}let value: Option<CompactString> = if values.is_empty() {{ None }} else {{ Some(values.join({sep:?}).into()) }};"
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
/// variables), indent, and enclosing hub/source/struct context. The items are
/// counted first (the iterator is a cheap copy) so the hub collection is
/// reserved once.
fn read_collection_block(out: &mut String, ctx: &GenCtx, coll: &SourceNode, frame: &Frame) {
    let coll_key = coll
        .canonical_key
        .as_deref()
        .expect("collection read block requires a canonical key");
    let src_item_struct =
        collection_item_struct(ctx.source, frame.parent_struct, &coll.source_path);
    let hub_item = item_struct_name(coll_key);
    let hub_field = snake_case(coll_key);
    let children = ctx.plan.children_of(&coll.id);
    let nested = ctx.plan.nested_collections_of(&coll.id);
    let clones = ctx.plan.clones_of(&coll.id);

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
    // Count the items first, so the hub collection is reserved once instead
    // of regrowing as items are pushed.
    let elements = format!("elements{depth}");
    let items = items_expr(
        ctx.source,
        frame.parent_struct,
        &coll.source_path,
        parent_src,
    );
    let _ = writeln!(out, "{pad}let {elements} = {items};");
    let _ = writeln!(out, "{pad}let {count} = {elements}.clone().count();");
    let _ = writeln!(out, "{pad}{parent_hub}.{hub_field}.reserve({count});");
    let _ = writeln!(out, "{pad}for ({idx}, {elem}) in {elements}.enumerate() {{");
    let _ = writeln!(
        out,
        "{body}let {elem} = Index::new({elem}, {src_item_struct}::SIZE);"
    );
    let _ = writeln!(out, "{body}let mut {item} = {hub_item}::default();");
    let child_alts = crate::alt::groups(children.iter().copied());
    let target = Target {
        struct_var: &item,
        parent_var: Some(parent_hub),
        base_var: &elem,
        index_var: Some(&idx),
        alts: &child_alts,
    };
    for child in children {
        out.push('\n');
        read_scalar_block(out, ctx, child, &src_item_struct, indent + 1, &target);
    }
    combine_alternatives(out, indent + 1, &target);
    for clone in clones {
        out.push('\n');
        read_clone_check_block(
            out,
            ctx,
            clone,
            &src_item_struct,
            indent + 1,
            &target,
            Some(parent_hub),
        );
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
                owned: false,
            },
        );
    }
    out.push('\n');
    let _ = writeln!(out, "{body}{parent_hub}.{hub_field}.push({item});");
    let _ = writeln!(out, "{pad}}}");

    // A required collection must have at least one item.
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
    normalize_chain(&access, normalize, false)
}

/// The expression reading a node's raw, normalized value. A `split` codec's
/// value is its two part elements joined (borrowed, as the parts are read
/// together); any other node reads its one source path.
fn node_value(ctx: &GenCtx, node: &SourceNode, start_struct: &str, target: &Target) -> String {
    if let Some(Pattern::Split { tail, .. }) = ctx.codec_of(node).map(|c| &c.pattern) {
        let head_path = &node.source_path;
        let tail_path = match head_path.rsplit_once('.') {
            Some((element, _)) => format!("{element}.{}", xml_field_name(tail)),
            None => xml_field_name(tail),
        };
        let head = access_expr(ctx.source, start_struct, head_path, target.base_var);
        let tail = access_expr(ctx.source, start_struct, &tail_path, target.base_var);
        return normalize_chain(
            &format!("codec::join_parts({head}, {tail})"),
            &node.normalize,
            true,
        );
    }
    read_one_value(
        ctx.source,
        start_struct,
        &node.source_path,
        &node.normalize,
        target,
    )
}

/// Emits the type-specific decode of `raw: String` into the typed `lhs` field.
/// With a `codec`, the raw lexical form is decoded into the canonical form
/// first (`CODEC_INVALID` when it does not match the codec's pattern).
#[allow(clippy::too_many_arguments)]
fn decode_body(
    out: &mut String,
    node: &SourceNode,
    codec: Option<&Codec>,
    key: &str,
    pad: &str,
    lhs: &str,
    target: &Target,
) {
    if let Some(codec) = codec {
        let (call, ok) = match (&codec.pattern, node.source_type) {
            (Pattern::Temporal(_), MappingType::Date) => (
                format!("codec::decode_date(raw.trim(), {:?})", codec.lexical),
                format!("Some(iso) => {lhs} = Some(iso),"),
            ),
            (Pattern::Temporal(_), MappingType::Datetime) => (
                format!("codec::decode_datetime(raw.trim(), {:?})", codec.lexical),
                format!("Some(iso) => {lhs} = Some(iso),"),
            ),
            (Pattern::Boolean { yes, no }, MappingType::Boolean) => (
                format!("codec::decode_bool(raw.trim(), {yes:?}, {no:?})"),
                format!("Some(b) => {lhs} = Some(b),"),
            ),
            (Pattern::Values(pairs), _) => {
                // First pair per wire value; an empty wire value is never read.
                let mut seen = BTreeSet::new();
                let arms: String = pairs
                    .iter()
                    .filter(|(_, wire)| !wire.is_empty() && seen.insert(wire.as_str()))
                    .map(|(canonical, wire)| format!("{wire:?} => Some({canonical:?}), "))
                    .collect();
                (
                    format!("match raw.trim() {{ {arms}_ => None }}"),
                    format!("Some(v) => {lhs} = Some(CompactString::from(v)),"),
                )
            }
            (Pattern::Fraction { .. }, MappingType::Decimal) => (
                "Decimal::from_str(raw.trim()).ok()".to_string(),
                format!("Some(d) => {lhs} = Some(d),"),
            ),
            // Latin-1 text, digits and a joined split value are read as they
            // are: nothing to decode, and the schema constrains the document.
            (Pattern::Latin1 | Pattern::Digits(_) | Pattern::Split { .. }, _) => {
                let _ = writeln!(out, "{pad}{lhs} = Some(raw);");
                return;
            }
            // Type / codec disagreement is validation's E085; nothing sane to emit.
            _ => (
                format!(
                    "compile_error!(\"codec `{}` does not fit a `{}` node\")",
                    codec.id, node.source_type
                ),
                String::new(),
            ),
        };
        let _ = writeln!(out, "{pad}match {call} {{");
        let _ = writeln!(out, "{pad}    {ok}");
        let _ = writeln!(out, "{pad}    None => {{");
        let msg = format!(
            "format!(\"`{{raw}}` does not match codec `{}` ({})\")",
            codec.id, codec.lexical
        );
        DiagSpec::new("Severity::Error", "CODEC_INVALID", node.id.as_str(), &msg)
            .key(key)
            .index(target.index_var)
            .emit(out, &format!("{pad}        "));
        let _ = writeln!(out, "{pad}    }}");
        let _ = writeln!(out, "{pad}}}");
        return;
    }
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

/// The Rust expression for a canonical literal of type `ty` (a `default`,
/// validated against the type at build time, E063).
fn typed_literal(ty: MappingType, value: &str) -> String {
    match ty {
        MappingType::Decimal => {
            format!("Decimal::from_str({value:?}).expect(\"validated at build time (E063)\")")
        }
        MappingType::Boolean => (value == "true").to_string(),
        _ => format!("CompactString::from({value:?})"),
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
