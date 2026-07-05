//! Source-path access expressions, shared by the reader and writer.
//!
//! Resolves a dotted `source_path` against the typed source model and builds the
//! Rust expression that reads it (honoring `Option<...>` segments), plus the
//! normalize-chain wrapper and the element-struct lookup for collections.

use crate::normalize::NormalizeOp;
use crate::source_model::{FieldType, SourceModelMeta};

/// Emits the Rust expression that yields `Option<&str>` for a scalar source path,
/// honoring `Option<...>` segments. The base is `source.<path>` at root scope, or
/// `element.<path>` inside a collection item.
pub(super) fn access_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    match walk_segments(source, start_struct, path) {
        Ok(segs) => build_access(base_var, path, &segs),
        // Unresolvable path: validation (E021/E023) should have rejected this
        // before codegen. Emit a loud `compile_error!` rather than plausible but
        // wrong access, so a codegen/validation gap fails at the exact site with a
        // clear message instead of a confusing downstream type error.
        Err(_) => unresolved(path, start_struct),
    }
}

/// Emits the Rust expression that yields `Option<String>` by *moving* the value
/// out of a mutable source struct (`take` for `Option` leaves, `mem::take` for
/// required ones). Only valid for paths read exactly once — a second read of a
/// taken path sees the leftover `Default`.
pub(super) fn take_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    match walk_segments(source, start_struct, path) {
        Ok(segs) => build_take(base_var, path, &segs),
        Err(_) => unresolved(path, start_struct),
    }
}

/// The loud failure expression for a source path codegen cannot resolve.
fn unresolved(path: &str, start_struct: &str) -> String {
    format!(
        "compile_error!(\"unresolved source path `{path}` against `{start_struct}` \
         (codegen bug or unvalidated mapping)\")"
    )
}

/// One resolved path segment: its name plus whether it is `Option`/`Vec` and
/// (for struct segments) the struct it descends into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SegInfo {
    pub(super) name: String,
    pub(super) optional: bool,
    pub(super) repeated: bool,
    pub(super) struct_name: Option<String>,
}

/// Walks `path` segment-by-segment from `start_struct`, returning per-segment
/// `Option`/`Vec`/struct info. Errors if any segment is unknown or a non-final
/// scalar.
pub(super) fn walk_segments(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
) -> Result<Vec<SegInfo>, ()> {
    let mut current = source.structs.get(start_struct).ok_or(())?;
    let segments: Vec<&str> = path.split('.').collect();
    let mut out = Vec::with_capacity(segments.len());

    for (i, seg) in segments.iter().enumerate() {
        let field = current.fields.get(*seg).ok_or(())?;
        let is_last = i + 1 == segments.len();
        let struct_name = match &field.ty {
            FieldType::Struct(name) => Some(name.clone()),
            FieldType::Scalar => None,
        };
        out.push(SegInfo {
            name: (*seg).to_string(),
            // Non-repeated interior structs are emitted as `Option<Box<…>>`,
            // so every chain through them is optional regardless of the
            // field's own optionality.
            optional: field.optional || (struct_name.is_some() && !field.repeated),
            repeated: field.repeated,
            struct_name: struct_name.clone(),
        });
        if !is_last {
            match struct_name {
                Some(name) => current = source.structs.get(&name).ok_or(())?,
                None => return Err(()),
            }
        }
    }
    Ok(out)
}

/// Builds an `Option<&str>` access expression from resolved segment info.
///
/// The builders below track the current *receiver* (`source`, then the
/// innermost closure variable): each optional interior opens an
/// `and_then` closure and the leaf expression is built on the receiver alone,
/// so a non-optional leaf under an optional interior wraps only itself, not
/// the whole chain.
fn build_access(base: &str, full_path: &str, segs: &[SegInfo]) -> String {
    let any_optional = segs.iter().any(|s| s.optional);
    if !any_optional {
        return format!("Some({base}.{full_path}.as_str())");
    }

    let mut expr = String::new();
    let mut recv = String::from(base);
    let mut depth = 0usize;
    let mut closers = String::new();
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        let place = format!("{recv}.{}", seg.name);
        if is_last {
            if seg.optional {
                let var = format!("v{depth}");
                expr.push_str(&format!(
                    "{place}.as_ref().and_then(|{var}| Some({var}.as_str()))"
                ));
            } else {
                expr.push_str(&format!("Some({place}.as_str())"));
            }
        } else if seg.optional {
            let var = format!("v{depth}");
            expr.push_str(&format!("{place}.as_ref().and_then(|{var}| "));
            recv = var;
            closers.push(')');
            depth += 1;
        } else {
            recv = place;
        }
    }
    format!("{expr}{closers}")
}

/// Builds an `Option<String>` *taking* expression from resolved segment info:
/// the structural mirror of [`build_access`] with `as_mut`/`take` in place of
/// `as_ref`/`as_str`, moving the leaf out and leaving `Default` behind.
fn build_take(base: &str, full_path: &str, segs: &[SegInfo]) -> String {
    let any_optional = segs.iter().any(|s| s.optional);
    if !any_optional {
        return format!("Some(std::mem::take(&mut {base}.{full_path}))");
    }

    let mut expr = String::new();
    let mut recv = String::from(base);
    let mut depth = 0usize;
    let mut closers = String::new();
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        let place = format!("{recv}.{}", seg.name);
        if is_last {
            if seg.optional {
                expr.push_str(&format!("{place}.take()"));
            } else {
                expr.push_str(&format!("Some(std::mem::take(&mut {place}))"));
            }
        } else if seg.optional {
            let var = format!("v{depth}");
            expr.push_str(&format!("{place}.as_mut().and_then(|{var}| "));
            recv = var;
            closers.push(')');
            depth += 1;
        } else {
            recv = place;
        }
    }
    format!("{expr}{closers}")
}

/// Emits the expression that *consumes* a `Vec` leaf at `path`, yielding the
/// `Vec` by value: a plain `mem::take` when no segment is optional, otherwise
/// the take chain with `.unwrap_or_default()` (an absent boxed interior yields
/// an empty `Vec` without materializing the subtree).
pub(super) fn collection_take_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    match walk_segments(source, start_struct, path) {
        Ok(segs) if segs.iter().any(|s| s.optional) => {
            format!("{}.unwrap_or_default()", build_take(base_var, path, &segs))
        }
        Ok(_) => format!("std::mem::take(&mut {base_var}.{path})"),
        Err(_) => unresolved(path, start_struct),
    }
}

/// Emits the expression that *borrows* a `Vec` leaf at `path` as a slice: a
/// plain reference when no segment is optional, otherwise an `as_ref` chain
/// ending in `.map_or(&[], …)`.
pub(super) fn collection_slice_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    let Ok(segs) = walk_segments(source, start_struct, path) else {
        return unresolved(path, start_struct);
    };
    if !segs.iter().any(|s| s.optional) {
        return format!("{base_var}.{path}.as_slice()");
    }
    // The leaf `Vec` itself is never optional; chain `and_then` through the
    // optional interiors and hand back the slice.
    let mut expr = String::new();
    let mut recv = String::from(base_var);
    let mut depth = 0usize;
    let mut closers = String::new();
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        let place = format!("{recv}.{}", seg.name);
        if is_last {
            expr.push_str(&format!("Some({place}.as_slice())"));
        } else if seg.optional {
            let var = format!("v{depth}");
            expr.push_str(&format!("{place}.as_ref().and_then(|{var}| "));
            recv = var;
            closers.push(')');
            depth += 1;
        } else {
            recv = place;
        }
    }
    format!("{expr}{closers}.unwrap_or(&[])")
}

/// Emits the mutable place expression for assigning into `path`: boxed
/// interior segments materialize on demand via `get_or_insert_default()`, the
/// leaf is a plain field access. Only valid in writer context, inside the
/// non-empty-value guard, so absent subtrees are never created for nothing.
pub(super) fn assign_target_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    let Ok(segs) = walk_segments(source, start_struct, path) else {
        return unresolved(path, start_struct);
    };
    let mut expr = String::from(base_var);
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        expr = format!("{expr}.{}", seg.name);
        if !is_last && seg.optional {
            expr.push_str(".get_or_insert_default()");
        }
    }
    expr
}

/// Wraps a base access in the node's normalize chain, yielding an
/// `Option<CompactString>`. `empty_as_missing` can collapse the value to
/// `None`.
///
/// When `owned` the access already yields `Option<CompactString>` (a moved-out
/// value) and is threaded through as-is; otherwise the access yields
/// `Option<&str>` and the chain copies once with a leading
/// `CompactString::from` (stack-only for values within the inline capacity).
/// Either way a single value moves through the helpers by value.
pub(super) fn normalize_chain(access: &str, ops: &[NormalizeOp], owned: bool) -> String {
    let mut expr = if owned {
        access.to_string()
    } else {
        format!("{access}.map(CompactString::from)")
    };
    for op in ops {
        match op {
            NormalizeOp::Trim => expr = format!("{expr}.map(normalize::trim)"),
            NormalizeOp::Uppercase => expr = format!("{expr}.map(normalize::uppercase)"),
            NormalizeOp::Lowercase => expr = format!("{expr}.map(normalize::lowercase)"),
            // `empty_as_missing` already returns `Option<String>`.
            NormalizeOp::EmptyAsMissing => {
                expr = format!("{expr}.and_then(normalize::empty_as_missing)")
            }
        }
    }
    expr
}

/// The element struct a collection field's source `path` resolves to, walked from
/// `start_struct` (the model root for a root collection, or the enclosing item
/// struct for a nested one).
pub(super) fn collection_item_struct(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
) -> String {
    walk_segments(source, start_struct, path)
        .ok()
        .and_then(|segs| segs.last().and_then(|s| s.struct_name.clone()))
        .unwrap_or_else(|| source.root.clone())
}
