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
        Err(_) => format!(
            "compile_error!(\"unresolved source path `{path}` against `{start_struct}` \
             (codegen bug or unvalidated mapping)\")"
        ),
    }
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
            optional: field.optional,
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
fn build_access(base: &str, full_path: &str, segs: &[SegInfo]) -> String {
    let any_optional = segs.iter().any(|s| s.optional);
    if !any_optional {
        return format!("Some({base}.{full_path}.as_str())");
    }

    let mut expr = String::from(base);
    let mut depth = 0usize;
    let mut closers = String::new();
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        if seg.optional {
            let var = format!("v{depth}");
            expr = format!("{expr}.{}.as_ref().and_then(|{var}| ", seg.name);
            if is_last {
                expr.push_str(&format!("Some({var}.as_str())"));
            } else {
                expr.push_str(&var);
            }
            closers.push(')');
            depth += 1;
        } else {
            expr = format!("{expr}.{}", seg.name);
            if is_last {
                expr = format!("Some({expr}.as_str())");
            }
        }
    }
    format!("{expr}{closers}")
}

/// Wraps a base `Option<&str>` access in the node's normalize chain, yielding an
/// `Option<String>`. `empty_as_missing` can collapse the value to `None`.
///
/// The chain owns a single `String` (allocated once by the leading
/// `s.to_string()`) and threads it through the normalize helpers by value, so no
/// step after the first reallocates: `trim` edits in place, the case folds reuse
/// the buffer's contents, and `empty_as_missing` hands it straight back.
pub(super) fn normalize_chain(access: &str, ops: &[NormalizeOp]) -> String {
    let mut expr = format!("{access}.map(|s| s.to_string())");
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
