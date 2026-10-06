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
//! optional `inherits` and `description` default to `None`, and `disabled`
//! (inherit-only base, emits no spoke) defaults to `false`. Unknown keys are rejected
//! (E001), so unsupported metadata never enters the compiler pipeline.
//!
//! # Namespaces
//!
//! Reading is namespace-agnostic (the deserializer matches local names), but a
//! written document must declare its namespaces and qualify its elements to be
//! schema-valid. Three optional `[meta]` entries describe that, and all three
//! are **inherited** from the parent mapping when a child omits them:
//!
//! - `root_ns` — the prefix of the root element (`""`, the default, means the
//!   default namespace / no prefix).
//! - `[meta.namespaces]` — `prefix = "URI"` pairs, every one declared on the
//!   root element as `xmlns:prefix` (`""` declares the default `xmlns`).
//! - `[meta.ns_defaults]` — `leaf` and `aggregate`: the prefix every scalar /
//!   valued element and every interior / collection element gets unless its
//!   node says `ns = "…"` itself.
//!
//! # Schema conformance
//!
//! Two more optional tables let a mapping declare the schema its format is
//! defined by and the documents that prove the mapping right, so the build
//! derives the conformance checks instead of hand-written output tests:
//!
//! - `[meta.schema]` ([`SchemaMeta`]) — the root XSD (`xsd`), an optional XML
//!   catalog resolving its imports offline (`catalog`), and `known_gaps`: the
//!   schema errors the spoke's output is documented to still produce.
//!   **Inherited** like the namespace entries (a CIUS validates against its
//!   base syntax's schema); a child may override it whole.
//! - `[[meta.samples]]` ([`SampleMeta`]) — documents to validate and round-trip
//!   through every emitting spoke. Read by the declaring spoke unless `source`
//!   names another one. Never inherited.
//!
//! Paths are workspace-relative; the loader checks they exist.
//!
//! # Document identity
//!
//! `[meta.identity]` ([`IdentityMeta`]) states what a document must declare to
//! be read by the spoke: the exact specification (profile) identifiers it
//! supports, the versions it supports, and the mandatory identity attributes of
//! the root. Together with the root's namespace URI (`[meta.namespaces]` at
//! `root_ns`) and local name (`root`), it is checked on every read — whether
//! the source format was auto-detected or named explicitly — and it is what
//! auto-detection matches. **Inherited** whole, like `[meta.schema]`.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The `[meta.ns_defaults]` table: the prefixes elements get when their node
/// declares no `ns` of its own.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NsDefaults {
    /// Prefix for scalar and valued elements (UBL: `cbc`). Default: none.
    #[serde(default)]
    pub leaf: Option<String>,
    /// Prefix for inferred interior elements and collection elements (UBL:
    /// `cac`). Default: none.
    #[serde(default)]
    pub aggregate: Option<String>,
}

/// The `[meta.schema]` table: the XSD the format is defined by, which the
/// spoke's emitted documents (and the samples it reads) are validated against.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMeta {
    /// The root XSD, relative to the workspace root.
    pub xsd: String,
    /// An XML catalog (`XML_CATALOG_FILES`) resolving the schema's remote
    /// imports offline, relative to the workspace root. Default: none.
    #[serde(default)]
    pub catalog: Option<String>,
    /// Substring patterns of the schema errors the spoke's output is known to
    /// still produce (a documented gap). Every reported error must match one,
    /// and every pattern must still match some error, so the list only shrinks.
    #[serde(default)]
    pub known_gaps: Vec<String>,
    /// Sample documents (workspace-relative, as `[[meta.samples]]` declares
    /// them) this spoke is documented to refuse: their data cannot be
    /// represented in its format, so the transform ends in error diagnostics
    /// instead of a document. A listed sample that writes cleanly is a stale
    /// entry and fails the check, so the list only shrinks.
    #[serde(default)]
    pub refuses: Vec<String>,
}

/// The `[meta.identity]` table: what a document must declare about itself to
/// be read by the spoke. Every value is matched exactly (after trimming the
/// element text) — no substrings, no case folding.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityMeta {
    /// Dotted element path, under the root, of the element whose text names
    /// the specification the document follows (UBL `CustomizationID`, CII
    /// `ExchangedDocumentContext.GuidelineSpecifiedDocumentContextParameter.ID`,
    /// EN 16931 BT-24). When set, the element is mandatory and its text must be
    /// one of [`Self::profiles`].
    #[serde(default)]
    pub profile: Option<String>,
    /// The exact profile identifiers the spoke supports.
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Dotted element path, under the root, of an optional version element
    /// (UBL `UBLVersionID`). When the document carries it, its text must be one
    /// of [`Self::versions`].
    #[serde(default)]
    pub version: Option<String>,
    /// The exact version values the spoke supports.
    #[serde(default)]
    pub versions: Vec<String>,
    /// Mandatory unqualified attributes of the root element and the exact values
    /// each may take (FatturaPA `versione`).
    #[serde(default)]
    pub attributes: BTreeMap<String, Vec<String>>,
}

/// One `[[meta.samples]]` entry: a document that must be schema-valid itself and
/// must round-trip through every emitting spoke.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleMeta {
    /// The document, relative to the workspace root.
    pub file: String,
    /// The spoke that reads it, when it is not the declaring spoke's own format:
    /// a mapping id (`xrechnung-invoice:3.0.2`) or a bare `doc_format`
    /// (`xrechnung-invoice`). Default: the declaring spoke.
    #[serde(default)]
    pub source: Option<String>,
}

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
    /// Prefix of the root element (`""` = default namespace / unprefixed).
    /// Inherited from the parent mapping when omitted.
    #[serde(default)]
    pub root_ns: Option<String>,
    /// Namespace declarations emitted on the root element: prefix → URI, with
    /// `""` for the default namespace. Inherited from the parent when omitted.
    #[serde(default)]
    pub namespaces: Option<BTreeMap<String, String>>,
    /// Default prefixes for leaf and aggregate elements. Inherited from the
    /// parent when omitted.
    #[serde(default)]
    pub ns_defaults: Option<NsDefaults>,
    /// The schema the format is defined by, for conformance checks. Inherited
    /// whole from the parent when omitted.
    #[serde(default)]
    pub schema: Option<SchemaMeta>,
    /// Sample documents that must be schema-valid and round-trip through every
    /// emitting spoke. Never inherited.
    #[serde(default)]
    pub samples: Vec<SampleMeta>,
    /// What a document must declare to be read by this spoke (profile,
    /// version, root identity attributes). Inherited whole from the parent
    /// when omitted.
    #[serde(default)]
    pub identity: Option<IdentityMeta>,
}

impl MappingMeta {
    /// Fills in the namespace entries (`root_ns`, `namespaces`, `ns_defaults`)
    /// this meta omits from `parent`, so a CIUS declares them once in its base.
    /// Each entry is inherited whole: a child's `[meta.namespaces]` replaces,
    /// never merges with, the parent's.
    pub fn inherit_namespaces(&mut self, parent: &MappingMeta) {
        if self.root_ns.is_none() {
            self.root_ns = parent.root_ns.clone();
        }
        if self.namespaces.is_none() {
            self.namespaces = parent.namespaces.clone();
        }
        if self.ns_defaults.is_none() {
            self.ns_defaults = parent.ns_defaults.clone();
        }
    }

    /// Fills in `[meta.schema]` from `parent` when this meta omits it, so a
    /// CIUS validates against its base syntax's schema. Inherited whole: a
    /// child's own table replaces, never merges with, the parent's. Samples are
    /// never inherited.
    pub fn inherit_schema(&mut self, parent: &MappingMeta) {
        if self.schema.is_none() {
            self.schema = parent.schema.clone();
        }
    }

    /// Fills in `[meta.identity]` from `parent` when this meta omits it.
    /// Inherited whole: a child's own table replaces the parent's.
    pub fn inherit_identity(&mut self, parent: &MappingMeta) {
        if self.identity.is_none() {
            self.identity = parent.identity.clone();
        }
    }

    /// The namespace URI of the root element: the `[meta.namespaces]` entry of
    /// its prefix (`root_ns`), or `None` when the root is in no namespace.
    pub fn root_namespace(&self) -> Option<&str> {
        self.namespaces
            .as_ref()
            .and_then(|ns| ns.get(self.root_prefix()))
            .map(String::as_str)
    }

    /// The effective root prefix (`""` when unset).
    pub fn root_prefix(&self) -> &str {
        self.root_ns.as_deref().unwrap_or("")
    }

    /// The effective default prefix for leaf elements (`""` when unset).
    pub fn leaf_prefix(&self) -> &str {
        self.ns_defaults
            .as_ref()
            .and_then(|d| d.leaf.as_deref())
            .unwrap_or("")
    }

    /// The effective default prefix for aggregate elements (`""` when unset).
    pub fn aggregate_prefix(&self) -> &str {
        self.ns_defaults
            .as_ref()
            .and_then(|d| d.aggregate.as_deref())
            .unwrap_or("")
    }

    /// The declared namespaces (empty when none are declared).
    pub fn declared_namespaces(&self) -> BTreeMap<String, String> {
        self.namespaces.clone().unwrap_or_default()
    }
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
    fn test_identity_defaults_none_and_parses() {
        let meta: MappingMeta = toml::from_str(required_only()).unwrap();
        assert_eq!(meta.identity, None);

        let src = format!(
            "{}\n[identity]\nprofile = \"CustomizationID\"\nprofiles = [\"urn:a\"]\nversion = \"UBLVersionID\"\nversions = [\"2.1\"]\n[identity.attributes]\nversione = [\"FPA12\"]",
            required_only()
        );
        let meta: MappingMeta = toml::from_str(&src).unwrap();
        let identity = meta.identity.expect("identity");
        assert_eq!(identity.profile.as_deref(), Some("CustomizationID"));
        assert_eq!(identity.profiles, ["urn:a"]);
        assert_eq!(identity.version.as_deref(), Some("UBLVersionID"));
        assert_eq!(identity.versions, ["2.1"]);
        assert_eq!(identity.attributes["versione"], ["FPA12"]);
    }

    #[test]
    fn test_detect_is_rejected() {
        // Substring markers were replaced by exact `[meta.identity]` values.
        let src = format!("{}\ndetect = [\"xrechnung\"]", required_only());
        assert!(toml::from_str::<MappingMeta>(&src).is_err());
    }

    #[test]
    fn test_inherit_identity_and_root_namespace() {
        let parent: MappingMeta = toml::from_str(&format!(
            "{}\nroot_ns = \"p\"\n[namespaces]\np = \"urn:p\"\n[identity]\nprofile = \"X\"\nprofiles = [\"a\"]",
            required_only()
        ))
        .unwrap();
        assert_eq!(parent.root_namespace(), Some("urn:p"));
        let mut child: MappingMeta = toml::from_str(required_only()).unwrap();
        assert_eq!(child.root_namespace(), None);
        child.inherit_identity(&parent);
        assert_eq!(child.identity, parent.identity);
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
    fn test_namespace_entries_default_to_none_and_parse() {
        let meta: MappingMeta = toml::from_str(required_only()).unwrap();
        assert_eq!(meta.root_ns, None);
        assert_eq!(meta.namespaces, None);
        assert_eq!(meta.ns_defaults, None);
        assert_eq!(meta.root_prefix(), "");
        assert_eq!(meta.leaf_prefix(), "");
        assert_eq!(meta.aggregate_prefix(), "");
        assert!(meta.declared_namespaces().is_empty());

        let src = format!(
            "{}\nroot_ns = \"rsm\"\n[namespaces]\n\"\" = \"urn:default\"\nrsm = \"urn:rsm\"\n[ns_defaults]\nleaf = \"ram\"\naggregate = \"ram\"",
            required_only()
        );
        let meta: MappingMeta = toml::from_str(&src).unwrap();
        assert_eq!(meta.root_prefix(), "rsm");
        assert_eq!(meta.leaf_prefix(), "ram");
        assert_eq!(meta.aggregate_prefix(), "ram");
        let ns = meta.declared_namespaces();
        assert_eq!(ns[""], "urn:default");
        assert_eq!(ns["rsm"], "urn:rsm");
    }

    #[test]
    fn test_unknown_ns_defaults_key_is_rejected() {
        let src = format!("{}\n[ns_defaults]\nbogus = \"x\"", required_only());
        assert!(toml::from_str::<MappingMeta>(&src).is_err());
    }

    #[test]
    fn test_inherit_namespaces_fills_only_omitted_entries() {
        let parent_src = format!(
            "{}\nroot_ns = \"p\"\n[namespaces]\np = \"urn:p\"\n[ns_defaults]\nleaf = \"cbc\"",
            required_only()
        );
        let parent: MappingMeta = toml::from_str(&parent_src).unwrap();
        let child_src = format!("{}\n[namespaces]\nq = \"urn:q\"", required_only());
        let mut child: MappingMeta = toml::from_str(&child_src).unwrap();
        child.inherit_namespaces(&parent);
        assert_eq!(child.root_prefix(), "p", "omitted: inherited");
        assert_eq!(child.leaf_prefix(), "cbc", "omitted: inherited");
        let ns = child.declared_namespaces();
        assert_eq!(ns.len(), 1, "declared: replaces the parent's table whole");
        assert_eq!(ns["q"], "urn:q");
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
    fn test_schema_and_samples_default_empty_and_parse() {
        let meta: MappingMeta = toml::from_str(required_only()).unwrap();
        assert_eq!(meta.schema, None);
        assert!(meta.samples.is_empty());

        let src = format!(
            "{}\n[schema]\nxsd = \"xsd/root.xsd\"\ncatalog = \"xsd/catalog.xml\"\nknown_gaps = [\"Expected is ( Header )\"]\n\n[[samples]]\nfile = \"a.xml\"\n\n[[samples]]\nfile = \"b.xml\"\nsource = \"other-fmt\"",
            required_only()
        );
        let meta: MappingMeta = toml::from_str(&src).unwrap();
        let schema = meta.schema.expect("schema");
        assert_eq!(schema.xsd, "xsd/root.xsd");
        assert_eq!(schema.catalog.as_deref(), Some("xsd/catalog.xml"));
        assert_eq!(schema.known_gaps, ["Expected is ( Header )"]);
        assert_eq!(meta.samples.len(), 2);
        assert_eq!(meta.samples[0].file, "a.xml");
        assert_eq!(meta.samples[0].source, None);
        assert_eq!(meta.samples[1].source.as_deref(), Some("other-fmt"));
    }

    #[test]
    fn test_schema_requires_xsd_and_rejects_unknown_keys() {
        let src = format!("{}\n[schema]\ncatalog = \"c.xml\"", required_only());
        assert!(toml::from_str::<MappingMeta>(&src).is_err(), "xsd missing");
        let src = format!("{}\n[schema]\nxsd = \"x.xsd\"\nbogus = 1", required_only());
        assert!(toml::from_str::<MappingMeta>(&src).is_err(), "unknown key");
        let src = format!("{}\n[[samples]]\nsource = \"x\"", required_only());
        assert!(toml::from_str::<MappingMeta>(&src).is_err(), "file missing");
    }

    #[test]
    fn test_inherit_schema_fills_only_an_omitted_table() {
        let parent_src = format!(
            "{}\n[schema]\nxsd = \"base.xsd\"\nknown_gaps = [\"gap\"]\n[[samples]]\nfile = \"base.xml\"",
            required_only()
        );
        let parent: MappingMeta = toml::from_str(&parent_src).unwrap();
        let mut child: MappingMeta = toml::from_str(required_only()).unwrap();
        child.inherit_schema(&parent);
        assert_eq!(child.schema, parent.schema, "omitted: inherited whole");
        assert!(child.samples.is_empty(), "samples are never inherited");

        let own_src = format!("{}\n[schema]\nxsd = \"own.xsd\"", required_only());
        let mut own: MappingMeta = toml::from_str(&own_src).unwrap();
        own.inherit_schema(&parent);
        let schema = own.schema.expect("schema");
        assert_eq!(schema.xsd, "own.xsd", "declared: replaces the parent's");
        assert!(schema.known_gaps.is_empty(), "replaced whole, not merged");
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
