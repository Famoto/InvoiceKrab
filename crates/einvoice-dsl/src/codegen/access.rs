//! Source-path access expressions, shared by the reader and writer.
//!
//! A source document is a packed record ([`einvoice_transformator::record`]):
//! every value a mapping can read or write has a numeric id in its record's
//! layout, which this module computes from the source model and resolves
//! dotted `source_path`s against.
//!
//! # Layout
//!
//! A struct laid out at `base` takes the ids `base..base + size`: `base`
//! itself (the element's own id, a presence mark), then one id per field in
//! emission order ([`StructMeta::ordered_fields`](crate::source_model::StructMeta::ordered_fields)).
//! A scalar (single or repeated) and a repeated element take one id; a
//! non-repeated interior element is laid out inline, taking its own struct's
//! size. Items of a repeated element are records of their own (laid out at
//! 0), nested under its id. A logical (aliased) field is never on the wire:
//! the writer lays a single one out inline and nests a repeated one's items
//! under its id, and `mux` moves them into the physical element; the reader
//! *picks* them from the physical element's items by their selector.
//!
//! # Structure
//!
//! - [`size`] / [`fields`] — the layout.
//! - [`resolve`] — a path as [`Step`]s.
//! - Read-side expressions: [`access_expr`], [`items_expr`], [`values_expr`],
//!   [`presence_expr`]. Write-side: [`write_access_expr`], [`slot`],
//!   [`owner_nonempty_expr`].
//! - [`normalize_chain`], [`walk_segments`], [`collection_item_struct`]: the
//!   model walk and the normalize wrapper.

use crate::normalize::NormalizeOp;
use crate::source_model::{FieldMeta, FieldType, SourceModelMeta};

/// Emits the loud failure expression for a source path codegen cannot
/// resolve. Validation (E021/E023) rejects such paths before codegen, so a
/// codegen/validation gap fails at the exact site instead of compiling into
/// plausible but wrong access.
fn unresolved(path: &str, start_struct: &str) -> String {
    format!(
        "compile_error!(\"unresolved source path `{path}` against `{start_struct}` \
         (codegen bug or unvalidated mapping)\")"
    )
}

/// The ids a field takes in its struct's layout: an element laid out inline
/// (an interior, or a single logical field) takes its struct's size.
fn width(source: &SourceModelMeta, field: &FieldMeta) -> u32 {
    match &field.ty {
        FieldType::Struct(inner) if !field.repeated => size(source, inner),
        _ => 1,
    }
}

/// The ids the struct `name` takes when laid out inline: its own, then its
/// fields'.
///
/// ponytail: recomputed per call (a walk of the subtree); memoize if build
/// times ever show it.
pub(super) fn size(source: &SourceModelMeta, name: &str) -> u32 {
    let Some(meta) = source.structs.get(name) else {
        return 1;
    };
    1 + meta.fields.values().map(|f| width(source, f)).sum::<u32>()
}

/// The struct's fields in emission order, each with its id relative to the
/// struct's own.
pub(super) fn fields<'a>(
    source: &SourceModelMeta,
    meta: &'a crate::source_model::StructMeta,
) -> Vec<(&'a String, &'a FieldMeta, u32)> {
    let mut at = 1;
    meta.ordered_fields()
        .into_iter()
        .map(|(name, field)| {
            let id = at;
            at += width(source, field);
            (name, field, id)
        })
        .collect()
}

/// One resolved path segment, its ids relative to the record it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    /// An element laid out inline in the current record (an interior, and on
    /// the write side a single logical field): the ids `lo..hi`.
    Inline { lo: u32, hi: u32, item: String },
    /// Read side: the items of the physical element `phys` that the logical
    /// field `index` of the owner's group picks (`owner::class_<physical>`);
    /// ids restart in the item.
    Pick {
        phys: u32,
        class: String,
        index: usize,
        repeated: bool,
        item: String,
    },
    /// A repeated element's items (on the write side also a repeated logical
    /// field's), nested under `id`.
    Items { id: u32, item: String },
    /// A scalar.
    Value {
        id: u32,
        repeated: bool,
        optional: bool,
    },
}

/// Which mapper a path is resolved for: the reader picks logical items from
/// their physical element, the writer has them laid out in its own record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Read,
    Write,
}

/// Resolves `path` from `start_struct` into one [`Step`] per segment; ids
/// restart after a [`Step::Pick`]. Errors on an unknown segment or one that
/// descends into a scalar or a repeated element.
pub(super) fn resolve(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    side: Side,
) -> Result<Vec<Step>, ()> {
    let mut current = start_struct.to_string();
    let mut base = 0u32;
    let mut out = Vec::new();
    let segments: Vec<&str> = path.split('.').collect();
    for (i, seg) in segments.iter().enumerate() {
        let meta = source.structs.get(&current).ok_or(())?;
        let laid = fields(source, meta);
        let (_, field, local) = laid.iter().find(|(n, _, _)| n.as_str() == *seg).ok_or(())?;
        let id = base + local;
        let step = match (&field.ty, &field.alias) {
            (FieldType::Scalar, _) => Step::Value {
                id,
                repeated: field.repeated,
                optional: field.optional,
            },
            (FieldType::Struct(inner), Some(alias)) if side == Side::Read => {
                let group = meta
                    .aliased_fields()
                    .into_iter()
                    .find(|(p, _)| **p == alias.physical)
                    .ok_or(())?;
                let index = group
                    .1
                    .iter()
                    .position(|(n, _)| n.as_str() == *seg)
                    .ok_or(())?;
                let phys = laid
                    .iter()
                    .find(|(n, _, _)| **n == alias.physical)
                    .map(|(_, _, local)| base + local)
                    .ok_or(())?;
                Step::Pick {
                    phys,
                    class: format!("{current}::class_{}", alias.physical),
                    index,
                    repeated: field.repeated,
                    item: inner.clone(),
                }
            }
            (FieldType::Struct(inner), _) if field.repeated => Step::Items {
                id,
                item: inner.clone(),
            },
            (FieldType::Struct(inner), _) => Step::Inline {
                lo: id,
                hi: id + size(source, inner),
                item: inner.clone(),
            },
        };
        if i + 1 < segments.len() {
            match &step {
                Step::Inline { item, .. } => current = item.clone(),
                Step::Pick {
                    repeated: false,
                    item,
                    ..
                } => {
                    current = item.clone();
                    base = 0;
                    out.push(step);
                    continue;
                }
                _ => return Err(()),
            }
            base = match &step {
                Step::Inline { lo, .. } => *lo,
                _ => unreachable!(),
            };
        }
        out.push(step);
    }
    Ok(out)
}

/// The `refs` argument a class function takes: the document values the
/// selectors compare against, when the mapping has any.
fn refs_arg(source: &SourceModelMeta) -> &'static str {
    if source.match_refs.is_empty() {
        ""
    } else {
        ", &refs"
    }
}

/// The read-side expression `leaf(receiver)` reached through `steps` (all
/// but the leaf) from `base`: every picked item opens an `and_then` closure,
/// so `leaf` must yield an `Option`, and so does the whole expression.
fn chain(
    source: &SourceModelMeta,
    base: &str,
    steps: &[Step],
    leaf: impl FnOnce(&str) -> String,
) -> (String, bool) {
    let mut expr = String::new();
    let mut recv = base.to_string();
    let mut closers = String::new();
    for (depth, step) in steps.iter().enumerate() {
        if let Step::Pick {
            phys, class, index, ..
        } = step
        {
            let var = format!("v{depth}");
            expr.push_str(&format!(
                "{recv}.items({phys}).find(|i| {class}(*i{}) == {index}).and_then(|{var}| ",
                refs_arg(source)
            ));
            recv = var;
            closers.push(')');
        }
    }
    let picked = !closers.is_empty();
    (format!("{expr}{}{closers}", leaf(&recv)), picked)
}

/// Emits the `Option<&str>` expression reading the scalar at `path` from the
/// record `base_var` (`source` at root, `element<n>` in a collection item).
pub(super) fn access_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    match resolve(source, start_struct, path, Side::Read) {
        Ok(steps) => match steps.split_last() {
            Some((
                Step::Value {
                    id,
                    repeated: false,
                    ..
                },
                rest,
            )) => chain(source, base_var, rest, |recv| format!("{recv}.get({id})")).0,
            _ => unresolved(path, start_struct),
        },
        Err(_) => unresolved(path, start_struct),
    }
}

/// Emits the iterator over the items (`Rec`s) of the repeated element at
/// `path`: a repeated element's nested items, or the physical items a
/// logical field picks. The iterator is `Clone`, so it can be counted first.
pub(super) fn items_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    let Ok(steps) = resolve(source, start_struct, path, Side::Read) else {
        return unresolved(path, start_struct);
    };
    let Some((last, rest)) = steps.split_last() else {
        return unresolved(path, start_struct);
    };
    let leaf = |recv: &str| match last {
        Step::Items { id, .. } => format!("{recv}.items({id})"),
        Step::Pick {
            phys,
            class,
            index,
            repeated: true,
            ..
        } => format!(
            "{recv}.items({phys}).filter(move |i| {class}(*i{}) == {index})",
            refs_arg(source)
        ),
        _ => unresolved(path, start_struct),
    };
    let (expr, picked) = chain(source, base_var, rest, |recv| {
        if rest.iter().any(|s| matches!(s, Step::Pick { .. })) {
            format!("Some({})", leaf(recv))
        } else {
            leaf(recv)
        }
    });
    if picked {
        format!("{expr}.into_iter().flatten()")
    } else {
        expr
    }
}

/// Emits the iterator over the values (`&str`) of the repeated scalar at
/// `path`.
pub(super) fn values_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    let Ok(steps) = resolve(source, start_struct, path, Side::Read) else {
        return unresolved(path, start_struct);
    };
    match steps.split_last() {
        Some((Step::Value { id, .. }, rest)) => {
            let picked = rest.iter().any(|s| matches!(s, Step::Pick { .. }));
            let (expr, _) = chain(source, base_var, rest, |recv| {
                if picked {
                    format!("Some({recv}.all({id}))")
                } else {
                    format!("{recv}.all({id})")
                }
            });
            if picked {
                format!("{expr}.into_iter().flatten()")
            } else {
                expr
            }
        }
        _ => unresolved(path, start_struct),
    }
}

/// The `bool` expression testing whether the element or value at `path`
/// exists: a scalar has a value, an element is present (with content or
/// marked), a repeated element has an item.
pub(super) fn presence_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    let Ok(steps) = resolve(source, start_struct, path, Side::Read) else {
        return unresolved(path, start_struct);
    };
    let Some((last, rest)) = steps.split_last() else {
        return unresolved(path, start_struct);
    };
    let leaf = |recv: &str| match last {
        Step::Value {
            id,
            repeated: false,
            ..
        } => format!("{recv}.get({id}).is_some()"),
        Step::Value { id, .. } => format!("{recv}.all({id}).next().is_some()"),
        Step::Inline { lo, hi, .. } => format!("{recv}.any({lo}, {hi})"),
        Step::Items { id, .. } => format!("{recv}.items({id}).next().is_some()"),
        Step::Pick {
            phys, class, index, ..
        } => format!(
            "{recv}.items({phys}).any(|i| {class}(i{}) == {index})",
            refs_arg(source)
        ),
    };
    if rest.iter().any(|s| matches!(s, Step::Pick { .. })) {
        let (expr, _) = chain(source, base_var, rest, |recv| {
            format!("({}).then_some(())", leaf(recv))
        });
        format!("{expr}.is_some()")
    } else {
        leaf(base_var)
    }
}

/// Emits the write-side `Option<&str>` expression reading back the scalar at
/// `path` from the record being written.
pub(super) fn write_access_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    match slot(source, start_struct, path) {
        Ok(slot) => format!("{base_var}.get({})", slot.id),
        Err(e) => e,
    }
}

/// Where the writer puts the value or items at a path: the id in the record
/// being written, and the innermost element laid out inline on the way
/// (whose own id marks the path's elements present).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Slot {
    pub(super) id: u32,
    pub(super) inline: Option<u32>,
}

/// Resolves `path` for the writer, or the `compile_error!` expression.
pub(super) fn slot(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
) -> Result<Slot, String> {
    let steps = resolve(source, start_struct, path, Side::Write)
        .map_err(|_| unresolved(path, start_struct))?;
    let inline = steps.iter().rev().skip(1).find_map(|s| match s {
        Step::Inline { lo, .. } => Some(*lo),
        _ => None,
    });
    match steps.last() {
        Some(Step::Value { id, .. } | Step::Items { id, .. }) => Ok(Slot { id: *id, inline }),
        Some(Step::Inline { lo, .. }) => Ok(Slot { id: *lo, inline }),
        _ => Err(unresolved(path, start_struct)),
    }
}

/// The write-side `bool` expression testing that the element at `path` (an
/// owner) holds content: an inline element with a non-empty value or an
/// item, a repeated element with an item.
pub(super) fn owner_nonempty_expr(
    source: &SourceModelMeta,
    start_struct: &str,
    path: &str,
    base_var: &str,
) -> String {
    match resolve(source, start_struct, path, Side::Write).as_deref() {
        Ok([.., Step::Inline { lo, item, .. }]) => {
            format!("!{item}::is_empty({base_var}.rec(), {lo})")
        }
        Ok([.., Step::Items { id, .. }]) => format!("{base_var}.items({id}).next().is_some()"),
        _ => format!("compile_error!(\"unresolved owner path `{path}` against `{start_struct}`\")"),
    }
}

/// One resolved model segment: its name plus whether it is `Option`/`Vec`
/// and (for struct segments) the struct it descends into.
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
            // A non-repeated interior element may be absent, so every chain
            // through it is optional regardless of the field's own flag.
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

/// Wraps a base access in the node's normalize chain, yielding an
/// `Option<CompactString>`. `empty_as_missing` can collapse the value to
/// `None`.
///
/// When `owned` the access already yields `Option<CompactString>` and is
/// threaded through as-is; otherwise the access yields `Option<&str>` and the
/// chain copies once with a leading `CompactString::from` (stack-only for
/// values within the inline capacity). Either way a single value moves
/// through the helpers by value.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_model::{SourceModelBuilder, ubl};
    use FieldType::{Scalar, Struct};

    const UNRESOLVED: &str = "compile_error!(\"unresolved source path";

    /// `Doc { lines: Vec<Line>, wrap: W }`, `W { lines: Vec<Line> }`,
    /// `Line { id: String }`: a collection directly on the root and one
    /// under an interior.
    fn nested_collections() -> SourceModelMeta {
        SourceModelBuilder::new("t:1", "Doc")
            .struct_def(
                "Doc",
                &[
                    ("lines", false, true, Struct("Line".into())),
                    ("wrap", false, false, Struct("W".into())),
                ],
            )
            .struct_def("W", &[("lines", false, true, Struct("Line".into()))])
            .struct_def("Line", &[("id", false, false, Scalar)])
            .build()
    }

    #[test]
    fn test_layout_lays_interiors_out_inline() {
        let src = nested_collections();
        assert_eq!(size(&src, "Line"), 2);
        assert_eq!(size(&src, "W"), 2);
        // Doc: own id 0, lines 1, wrap 2..4 (its own id 2, its lines 3).
        assert_eq!(size(&src, "Doc"), 4);
        assert_eq!(
            resolve(&src, "Doc", "wrap.lines", Side::Read),
            Ok(vec![
                Step::Inline {
                    lo: 2,
                    hi: 4,
                    item: "W".into()
                },
                Step::Items {
                    id: 3,
                    item: "Line".into()
                },
            ])
        );
        assert!(
            resolve(&src, "Doc", "lines.id", Side::Read).is_err(),
            "through a repeated element"
        );
        assert!(resolve(&src, "Nope", "id", Side::Read).is_err());
        assert!(resolve(&src, "Line", "id.deeper", Side::Read).is_err());
    }

    #[test]
    fn test_read_expressions() {
        let src = nested_collections();
        assert_eq!(
            access_expr(&src, "Line", "id", "element0"),
            "element0.get(1)"
        );
        assert_eq!(
            items_expr(&src, "Doc", "wrap.lines", "source"),
            "source.items(3)"
        );
        assert_eq!(presence_expr(&src, "Doc", "wrap", "item"), "item.any(2, 4)");
        assert_eq!(
            presence_expr(&src, "Doc", "lines", "item"),
            "item.items(1).next().is_some()"
        );
        assert!(access_expr(&src, "Doc", "missing", "source").starts_with(UNRESOLVED));
        assert!(items_expr(&src, "Doc", "nope", "source").starts_with(UNRESOLVED));
        let ubl = ubl();
        let deep = access_expr(
            &ubl,
            "Invoice",
            "monetary_total.payable_amount.value",
            "source",
        );
        assert!(
            deep.starts_with("source.get(") && !deep.contains("and_then"),
            "{deep}"
        );
    }

    #[test]
    fn test_write_slots() {
        let src = nested_collections();
        assert_eq!(
            slot(&src, "Doc", "wrap.lines"),
            Ok(Slot {
                id: 3,
                inline: Some(2)
            })
        );
        assert_eq!(
            slot(&src, "Doc", "lines"),
            Ok(Slot {
                id: 1,
                inline: None
            })
        );
        assert_eq!(
            write_access_expr(&src, "Line", "id", "element0"),
            "element0.get(1)"
        );
        assert_eq!(
            owner_nonempty_expr(&src, "Doc", "wrap", "source"),
            "!W::is_empty(source.rec(), 2)"
        );
        assert!(
            slot(&src, "Doc", "nope")
                .unwrap_err()
                .starts_with(UNRESOLVED)
        );
    }

    #[test]
    fn test_normalize_chain_applies_ops_in_order() {
        use NormalizeOp::*;
        assert_eq!(
            normalize_chain("a", &[Trim, Uppercase, Lowercase, EmptyAsMissing], false),
            "a.map(CompactString::from).map(normalize::trim).map(normalize::uppercase)\
             .map(normalize::lowercase).and_then(normalize::empty_as_missing)"
        );
        // An owned access is already `Option<CompactString>`: no copy.
        assert_eq!(normalize_chain("a", &[], true), "a");
    }

    #[test]
    fn test_collection_item_struct_falls_back_to_root() {
        let src = nested_collections();
        assert_eq!(collection_item_struct(&src, "Doc", "wrap.lines"), "Line");
        // A scalar leaf or an unresolvable path names no element struct.
        assert_eq!(collection_item_struct(&src, "Line", "id"), "Doc");
        assert_eq!(collection_item_struct(&src, "Doc", "nope"), "Doc");
    }
}
