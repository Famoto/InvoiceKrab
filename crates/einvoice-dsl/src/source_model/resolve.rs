//! The source-path resolver.
//!
//! Walks a dotted source path against a [`SourceModelMeta`], applying the
//! traversal rules (descend into structs, accumulate `Option`/`Vec` wrappers,
//! reject descending into a scalar) and reporting the resolved leaf as a
//! [`ResolvedField`] or a [`PathError`].

use super::meta::{FieldType, PathError, ResolvedField, SourceModelMeta};

/// Resolves a dotted `path` against `meta`, starting at the root struct.
pub fn resolve_path(meta: &SourceModelMeta, path: &str) -> Result<ResolvedField, PathError> {
    let root = meta.root.clone();
    resolve_path_from(meta, &root, path)
}

/// Resolves a dotted `path` against `meta`, starting at struct `start`.
///
/// Collection-child node paths are evaluated relative to the collection's item
/// struct, so the validator resolves them from the element
/// struct rather than the model root.
pub fn resolve_path_from(
    meta: &SourceModelMeta,
    start: &str,
    path: &str,
) -> Result<ResolvedField, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    let mut current = meta
        .structs
        .get(start)
        .ok_or_else(|| PathError::UnknownRoot(start.to_string()))?;
    let mut current_name = start;

    let mut repeated = false;
    let mut optional = false;

    let segments: Vec<&str> = path.split('.').collect();
    for (i, segment) in segments.iter().enumerate() {
        let field = current
            .fields
            .get(*segment)
            .ok_or_else(|| PathError::UnknownField {
                struct_name: current_name.to_string(),
                field: segment.to_string(),
            })?;
        repeated |= field.repeated;
        optional |= field.optional;

        let is_last = i + 1 == segments.len();
        match &field.ty {
            FieldType::Struct(name) => {
                if is_last {
                    return Ok(ResolvedField {
                        repeated,
                        optional,
                        is_struct: true,
                        struct_name: Some(name.clone()),
                    });
                }
                current = meta
                    .structs
                    .get(name)
                    .ok_or_else(|| PathError::UnknownRoot(name.clone()))?;
                current_name = name;
            }
            FieldType::Scalar => {
                if is_last {
                    return Ok(ResolvedField {
                        repeated,
                        optional,
                        is_struct: false,
                        struct_name: None,
                    });
                }
                return Err(PathError::NotAStruct {
                    struct_name: current_name.to_string(),
                    field: segment.to_string(),
                });
            }
        }
    }
    unreachable!("a non-empty path always returns inside the loop")
}

#[cfg(test)]
mod tests {
    use super::super::meta::ubl;
    use super::*;

    #[test]
    fn test_resolve_simple_scalar() {
        let r = resolve_path(&ubl(), "id").unwrap();
        assert_eq!(
            r,
            ResolvedField {
                repeated: false,
                optional: false,
                is_struct: false,
                struct_name: None
            }
        );
    }

    #[test]
    fn test_resolve_from_collection_item_struct() {
        // A collection child path resolves against the element struct, not root.
        let r = resolve_path_from(&ubl(), "InvoiceLine", "id").unwrap();
        assert!(!r.repeated && !r.is_struct);
        // The same path from root would also exist here, but a line-only field
        // must resolve from the item struct.
        assert!(resolve_path_from(&ubl(), "Invoice", "id").is_ok());
    }

    #[test]
    fn test_collection_field_exposes_element_struct_name() {
        let r = resolve_path(&ubl(), "invoice_lines").unwrap();
        assert_eq!(r.struct_name.as_deref(), Some("InvoiceLine"));
    }

    #[test]
    fn test_resolve_optional_is_tracked() {
        let r = resolve_path(&ubl(), "uuid").unwrap();
        assert!(r.optional);
        assert!(!r.repeated);
    }

    #[test]
    fn test_resolve_nested_scalar_path() {
        let r = resolve_path(&ubl(), "monetary_total.payable_amount.value").unwrap();
        assert!(!r.repeated && !r.optional && !r.is_struct);
    }

    #[test]
    fn test_resolve_through_vec_marks_repeated() {
        // A scalar under a Vec<struct> is `repeated` (multiple values / per item).
        let r = resolve_path(&ubl(), "invoice_lines.id").unwrap();
        assert!(r.repeated);
        assert!(!r.is_struct);
    }

    #[test]
    fn test_resolve_collection_field_is_struct() {
        let r = resolve_path(&ubl(), "invoice_lines").unwrap();
        assert!(r.repeated);
        assert!(r.is_struct);
    }

    #[test]
    fn test_unknown_field_errors() {
        let err = resolve_path(&ubl(), "nope").unwrap_err();
        assert_eq!(
            err,
            PathError::UnknownField {
                struct_name: "Invoice".into(),
                field: "nope".into()
            }
        );
    }

    #[test]
    fn test_descend_into_scalar_errors() {
        let err = resolve_path(&ubl(), "id.extra").unwrap_err();
        assert_eq!(
            err,
            PathError::NotAStruct {
                struct_name: "Invoice".into(),
                field: "id".into()
            }
        );
    }

    #[test]
    fn test_empty_path_errors() {
        assert_eq!(resolve_path(&ubl(), "").unwrap_err(), PathError::Empty);
    }
}
