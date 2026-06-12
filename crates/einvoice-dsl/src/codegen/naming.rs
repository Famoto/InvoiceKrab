//! Naming and type helpers shared across the codegen submodules.
//!
//! Converts canonical keys to Rust identifiers and maps canonical
//! [`MappingType`]s to their generated scalar types. These rules are local to
//! codegen and intentionally distinct from the XML-name/`doc_format` conversions
//! elsewhere (canonical keys are PascalCase with no acronym/digit handling).

use crate::types::MappingType;

/// Converts a PascalCase/`mixed` canonical key to a `snake_case` Rust field name
/// (e.g. `InvoiceNumber` → `invoice_number`, `LineId` → `line_id`).
pub(crate) fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The `MainKey` field name for a canonical key.
pub(crate) fn field_name(key: &str) -> String {
    snake_case(key)
}

/// The generated item-struct name for a canonical collection key (e.g.
/// `InvoiceLines` → `InvoiceLinesItem`).
pub(super) fn item_struct_name(coll_key: &str) -> String {
    format!("{coll_key}Item")
}

/// The Rust scalar type a canonical `MappingType` maps to.
pub(super) fn canonical_rust_type(ty: MappingType) -> &'static str {
    match ty {
        MappingType::Decimal => "Decimal",
        MappingType::Boolean => "bool",
        // string / identifier / currency / date / datetime / unit_code.
        _ => "String",
    }
}
