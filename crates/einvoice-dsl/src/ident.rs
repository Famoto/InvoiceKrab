//! Name rules shared by synthesis, validation and codegen.
//!
//! Mapping authors name things in two vocabularies the generated Rust must
//! absorb: XML names (node id segments and `xml` bindings, which become source
//! struct and field names) and canonical keys (which become hub fields and
//! item structs). This module holds the checks that keep both inside what the
//! generated code can express:
//!
//! - [`is_xml_name`] — an XML `NCName`, the only names a node may bind (E026).
//! - [`is_canonical_key`] — the `PascalCase` identifier shape of a key (E014).
//! - `escape_keyword` — turns a field name that is a Rust keyword into a
//!   usable identifier (`type` → `type_`).
//! - `RESERVED_TYPE_NAMES` — names the generated modules already use, which
//!   a synthesized struct must not take.

/// Every Rust keyword (strict, reserved and edition-2024 `gen`) that cannot be
/// used as a plain identifier.
const RUST_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "try", "type",
    "typeof", "unsafe", "unsized", "use", "virtual", "where", "while", "yield", "_",
];

/// Type and trait names the generated spoke modules use unqualified (the
/// prelude and their imports). A synthesized source struct with one of these
/// names would shadow it, so synthesis picks another name.
pub(crate) const RESERVED_TYPE_NAMES: &[&str] = &[
    // std prelude types, variants and traits the generated code names.
    "Option",
    "Some",
    "None",
    "Result",
    "Ok",
    "Err",
    "Vec",
    "String",
    "Box",
    "Default",
    "Clone",
    "Copy",
    "Debug",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "Iterator",
    "IntoIterator",
    "Into",
    "From",
    "ToString",
    "ToOwned",
    "AsRef",
    "Extend",
    "FromIterator",
    "Send",
    "Sync",
    "Sized",
    "Drop",
    "Fn",
    "FnMut",
    "FnOnce",
    "Self",
    // The generated modules' imports.
    "Serialize",
    "Deserialize",
    "FromStr",
    "CompactString",
    "ToCompactString",
    "Decimal",
    "MappingDiagnostic",
    "MappingResult",
    "Severity",
    "MainKey",
];

/// Whether `s` is a Rust keyword.
pub(crate) fn is_keyword(s: &str) -> bool {
    RUST_KEYWORDS.contains(&s)
}

/// `s` with a trailing `_` when it is a Rust keyword, so it can name a field
/// (`type` → `type_`, `ref` → `ref_`); any other name is returned unchanged.
pub(crate) fn escape_keyword(s: String) -> String {
    if is_keyword(&s) { s + "_" } else { s }
}

/// Whether `s` is an XML `NCName`: a non-colonized name, starting with a letter
/// or `_`, continuing with letters, digits, `-`, `.` or `_`. (Letters are
/// Unicode alphabetic characters; the full XML production also admits a few
/// combining ranges no invoice format uses.)
pub fn is_xml_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// Whether `s` is a well-formed canonical key: ASCII `PascalCase` (an upper-case
/// letter, then letters and digits) and not `Self`. Such a key is a valid Rust
/// type-name fragment for its `<Key>Item` struct, and its `snake_case` hub
/// field never needs more than keyword escaping.
pub fn is_canonical_key(s: &str) -> bool {
    let mut bytes = s.bytes();
    s != "Self"
        && bytes.next().is_some_and(|b| b.is_ascii_uppercase())
        && bytes.all(|b| b.is_ascii_alphanumeric())
}

/// Whether `s` is a plain ASCII Rust identifier usable as a type name: the
/// constraint on `[meta].root`, which names the root struct verbatim.
pub fn is_root_type_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !is_keyword(s)
        && !RESERVED_TYPE_NAMES.contains(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("ID", true)]
    #[case("currencyID", true)]
    #[case("my-elem", true)]
    #[case("a.b", true)]
    #[case("_x", true)]
    #[case("Ünï", true)]
    #[case("", false)]
    #[case("1st", false)]
    #[case("-x", false)]
    #[case("a b", false)]
    #[case("cbc:ID", false)]
    #[case("bad<>", false)]
    fn test_is_xml_name(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(is_xml_name(s), ok, "{s:?}");
    }

    #[rstest]
    #[case("InvoiceNumber", true)]
    #[case("LineId2", true)]
    #[case("Type", true)]
    #[case("", false)]
    #[case("Self", false)]
    #[case("invoiceNumber", false)]
    #[case("Foo_Bar", false)]
    #[case("Foo-Bar", false)]
    #[case("1Abc", false)]
    #[case("Über", false)]
    fn test_is_canonical_key(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(is_canonical_key(s), ok, "{s:?}");
    }

    #[test]
    fn test_escape_keyword_only_touches_keywords() {
        assert_eq!(escape_keyword("type".into()), "type_");
        assert_eq!(escape_keyword("ref".into()), "ref_");
        assert_eq!(escape_keyword("self".into()), "self_");
        assert_eq!(escape_keyword("type_".into()), "type_");
        assert_eq!(escape_keyword("id".into()), "id");
    }

    #[rstest]
    #[case("Invoice", true)]
    #[case("CrossIndustryInvoice", true)]
    #[case("my-root", false)]
    #[case("Self", false)]
    #[case("Option", false)]
    #[case("1Root", false)]
    fn test_is_root_type_name(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(is_root_type_name(s), ok, "{s:?}");
    }
}
