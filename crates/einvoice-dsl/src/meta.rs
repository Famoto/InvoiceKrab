//! The reserved `[meta]` table.
//!
//! # Structure
//!
//! - [`MappingMeta`] — the parsed `[meta]` table.
//!
//! # Behavior
//!
//! Required fields (`doc_format`, `format_version`, `mapping_version`,
//! `source_model`, `canonical_model`) are enforced by deserialization; the
//! optional `inherits` and `description` default to `None`, `detect`
//! (the auto-detection markers) defaults to empty, and `disabled` (inherit-only
//! base, emits no spoke) defaults to `false`. Unknown keys are rejected
//! (E001), so unsupported metadata never enters the compiler pipeline.

use serde::Deserialize;

/// The parsed `[meta]` table of a spoke mapping file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingMeta {
    /// Logical document format id (e.g. `ubl-invoice`).
    pub doc_format: String,
    /// Format version (e.g. `2.1`).
    pub format_version: String,
    /// Mapping file version (e.g. `1.0`).
    pub mapping_version: String,
    /// Canonical model id this mapping targets (e.g. `canonical-invoice:1.0`).
    pub canonical_model: String,
    /// The root source struct/XML element name (e.g. `Invoice`). The node ids
    /// mirror the XML tree under this root, and the compiler synthesizes the typed
    /// source struct from them. Defaults to `Root`
    /// when omitted.
    #[serde(default)]
    pub root: Option<String>,
    /// Optional source-model id label (e.g. `ubl-invoice:2.1`). Used as the
    /// synthesized model's id; defaults to `doc_format:format_version`.
    #[serde(default)]
    pub source_model: Option<String>,
    /// Optional discriminator substrings used to recognize this format when a
    /// caller must auto-detect the source format of a document. Matched
    /// case-insensitively against the document's `CustomizationID` (EN16931
    /// BT-24); a format whose marker is present is preferred over a generic
    /// format that declares none. A base format (e.g. plain UBL) leaves this
    /// empty and acts as the fallback.
    #[serde(default)]
    pub detect: Vec<String>,
    /// Optional parent mapping id this file inherits from.
    #[serde(default)]
    pub inherits: Option<String>,
    /// When true, this mapping is inherit-only: other spokes may reference it via
    /// `inherits`, but it does not itself emit a spoke (no `Spoke` variant, no
    /// read/write dispatch). Use for abstract base syntaxes (e.g. plain CII) that
    /// exist only to be specialized by a CIUS profile. Defaults to `false`.
    #[serde(default)]
    pub disabled: bool,
    /// Optional human description (reports only).
    #[serde(default)]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_only() -> &'static str {
        r#"
            doc_format = "ubl-invoice"
            format_version = "2.1"
            mapping_version = "1.0"
            source_model = "ubl-invoice:2.1"
            canonical_model = "canonical-invoice:1.0"
        "#
    }

    #[test]
    fn test_required_fields_parse_with_optionals_none() {
        let meta: MappingMeta = toml::from_str(required_only()).unwrap();
        assert_eq!(meta.doc_format, "ubl-invoice");
        assert_eq!(meta.source_model.as_deref(), Some("ubl-invoice:2.1"));
        assert_eq!(meta.canonical_model, "canonical-invoice:1.0");
        assert_eq!(meta.inherits, None);
        assert_eq!(meta.description, None);
    }

    #[test]
    fn test_detect_defaults_empty_and_parses_list() {
        let meta: MappingMeta = toml::from_str(required_only()).unwrap();
        assert!(meta.detect.is_empty());

        let src = format!("{}\ndetect = [\"xrechnung\", \"cius\"]", required_only());
        let meta: MappingMeta = toml::from_str(&src).unwrap();
        assert_eq!(
            meta.detect,
            vec!["xrechnung".to_string(), "cius".to_string()]
        );
    }

    #[test]
    fn test_optional_inherits_and_description_parse() {
        let src = format!(
            "{}\ninherits = \"ubl-invoice:2.0\"\ndescription = \"x\"",
            required_only()
        );
        let meta: MappingMeta = toml::from_str(&src).unwrap();
        assert_eq!(meta.inherits.as_deref(), Some("ubl-invoice:2.0"));
        assert_eq!(meta.description.as_deref(), Some("x"));
    }

    #[test]
    fn test_disabled_defaults_false_and_parses_true() {
        let meta: MappingMeta = toml::from_str(required_only()).unwrap();
        assert!(!meta.disabled);

        let src = format!("{}\ndisabled = true", required_only());
        let meta: MappingMeta = toml::from_str(&src).unwrap();
        assert!(meta.disabled);
    }

    #[test]
    fn test_missing_required_field_is_error() {
        let src = r#"
            doc_format = "ubl-invoice"
            format_version = "2.1"
            mapping_version = "1.0"
            source_model = "ubl-invoice:2.1"
        "#; // canonical_model missing
        assert!(toml::from_str::<MappingMeta>(src).is_err());
    }

    #[test]
    fn test_unknown_field_is_rejected() {
        let src = format!("{}\nextra = \"nope\"", required_only());
        assert!(toml::from_str::<MappingMeta>(&src).is_err());
    }
}
