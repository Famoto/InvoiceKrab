//! The transformation contract of a compiled spoke.
//!
//! A spoke's contract is the static summary a transform analysis needs: the
//! canonical keys it maps (with type, scope, codec and write-side pin), how each
//! `required` node obtains its written value (its *write route*), the read-side
//! collapses it declares (`multiple`), and its `match` selectors. Two contracts
//! determine how a `source → target` transform behaves, because the engine is
//! hub-and-spoke: nothing about a pair depends on a document.
//!
//! [`spoke_contract`] derives the contract from a compiled [`MappingIr`] and its
//! synthesized [`SourceModelMeta`]; [`render_contract`] emits it as the Rust
//! `static` initializer the generated registry embeds (the runtime types live
//! in the interfaces crate, which hands their module path in), and
//! [`check_required_routes`] is the build-time half of the `required` contract:
//! a key a spoke requires from the hub that no other spoke maps can never be
//! supplied by a transform, which is `W095`.
//!
//! # `required` means "has a write route"
//!
//! `required = true` on a mapped node promises that the written document will
//! carry the node's value. The compiler reads it as: the node has a
//! deterministic *write route* — its hub key (which the source of a transform
//! must map), a `constant`, or a resolved `clone_of`. Pair analysis checks the
//! route against a concrete source; [`check_required_routes`] checks it
//! against every other spoke at build time; the generated writer still reports
//! `REQUIRED_MISSING` when a document arrives without the value.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::error::{Diagnostic, Severity};
use crate::hub::{CanonicalScope, canonical_scope_of};
use crate::ir::MappingIr;
use crate::multiple::MultiplePolicy;
use crate::node::{DerivationScope, Scope, SourceNode};
use crate::report::FieldKey;
use crate::source_model::SourceModelMeta;

/// One spoke's transformation contract (the compile-time, owned form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokeContract {
    /// The spoke's display name.
    pub spoke: String,
    /// Every canonical key the spoke maps, sorted by label.
    pub keys: Vec<KeyContract>,
    /// Every `required` node's write route, in node-id order.
    pub required: Vec<RequiredRoute>,
    /// Read-side collapses (`multiple = "first" | "join"`), in node-id order.
    pub collapses: Vec<Collapse>,
    /// `match` selectors, in node-id order.
    pub selectors: Vec<Selector>,
}

/// One canonical key a spoke maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyContract {
    /// Scope-qualified label (`InvoiceLines/LineId`).
    pub label: String,
    /// The canonical key.
    pub key: String,
    /// Enclosing canonical collections, outermost first; empty at root.
    pub scope: Vec<String>,
    /// The semantic type name (`identifier`, `decimal`, …, `collection`).
    pub ty: String,
    /// The lexical codec, if any.
    pub codec: Option<String>,
    /// The write-side constant pinned on the keyed node, if any.
    pub pinned: Option<String>,
}

/// How a `required` node obtains its written value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// From the hub key with this label.
    Hub(String),
    /// A fixed literal.
    Constant(String),
    /// Mirrors the hub key with this label.
    Clone(String),
}

impl Route {
    /// The hub label the route depends on, if any.
    pub fn needs(&self) -> Option<&str> {
        match self {
            Route::Hub(label) | Route::Clone(label) => Some(label),
            Route::Constant(_) => None,
        }
    }
}

/// A `required` node and its write route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredRoute {
    /// The node id.
    pub node: String,
    /// Where the written value comes from.
    pub route: Route,
}

/// A read-side collapse of repeated values into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collapse {
    /// The node id.
    pub node: String,
    /// The label of the key it fills.
    pub label: String,
    /// `first` or `join`.
    pub policy: String,
}

/// A `match` selector on a shared physical element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    /// The logical node id.
    pub node: String,
    /// The physical element's local name.
    pub element: String,
    /// `(item field path, expected text)` pairs; empty for the default bucket.
    pub selector: Vec<(String, String)>,
    /// Whether the node keeps one occurrence (structural) rather than all.
    pub single: bool,
}

/// Derives `spoke`'s contract from its compiled IR and synthesized source model.
pub fn spoke_contract(spoke: &str, ir: &MappingIr, source: &SourceModelMeta) -> SpokeContract {
    let mut keys: Vec<KeyContract> = Vec::new();
    let mut required = Vec::new();
    let mut collapses = Vec::new();

    for node in ir.nodes.values() {
        let label = node
            .canonical_key
            .as_deref()
            .and_then(|key| key_label(node, ir, key));
        if let (Some(key), Some((label, scope))) = (&node.canonical_key, &label) {
            keys.push(KeyContract {
                label: label.clone(),
                key: key.clone(),
                scope: scope.clone(),
                ty: node.source_type.as_str().to_string(),
                codec: node.codec.clone(),
                pinned: node.constant.clone(),
            });
            if let Some(policy) = node.multiple.filter(|p| *p != MultiplePolicy::Error) {
                collapses.push(Collapse {
                    node: node.id.to_string(),
                    label: label.clone(),
                    policy: policy.as_str().to_string(),
                });
            }
        }
        if node.required
            && let Some(route) = write_route(node, ir, label.as_ref().map(|(l, _)| l.as_str()))
        {
            required.push(RequiredRoute {
                node: node.id.to_string(),
                route,
            });
        }
    }
    keys.sort_by(|a, b| a.label.cmp(&b.label));

    let mut selectors = Vec::new();
    for meta in source.structs.values() {
        for (physical, logical) in meta.aliased_fields() {
            let element = meta.fields[physical]
                .xml
                .clone()
                .unwrap_or_else(|| physical.clone());
            for (_, field) in logical {
                let Some(alias) = &field.alias else {
                    continue;
                };
                selectors.push(Selector {
                    node: alias.node.clone(),
                    element: element.clone(),
                    selector: alias.selector.clone(),
                    single: !field.repeated,
                });
            }
        }
    }
    selectors.sort_by(|a, b| a.node.cmp(&b.node));

    SpokeContract {
        spoke: spoke.to_string(),
        keys,
        required,
        collapses,
        selectors,
    }
}

/// The scope-qualified label and scope chain of `key` declared on `node`, or
/// `None` when an enclosing collection has no canonical key (E011 territory).
fn key_label(node: &SourceNode, ir: &MappingIr, key: &str) -> Option<(String, Vec<String>)> {
    let scope = canonical_scope_of(node, ir)?;
    let chain = match &scope {
        CanonicalScope::Root => Vec::new(),
        CanonicalScope::Collection(chain) => chain.clone(),
    };
    let label = FieldKey {
        scope,
        key: key.to_string(),
    }
    .label();
    Some((label, chain))
}

/// The write route of a required node: a constant beats the hub key (the
/// constant is what gets written), a clone follows its derivation to the key
/// it mirrors, and a plain keyed node needs its own key. A required helper
/// without any of these has no route (nothing is ever written for it).
fn write_route(node: &SourceNode, ir: &MappingIr, own_label: Option<&str>) -> Option<Route> {
    if let Some(value) = &node.constant {
        return Some(Route::Constant(value.clone()));
    }
    if let Some(Ok(derivation)) = node.derivation() {
        let scope = match derivation.scope {
            DerivationScope::Own => Some(node.scope.clone()),
            DerivationScope::Root => Some(Scope::Root),
            DerivationScope::Parent => match &node.scope {
                Scope::Collection(coll) => ir.nodes.get(coll).map(|c| c.scope.clone()),
                Scope::Root => None,
            },
        }?;
        // The label of the mirrored key is that of the primary declaring it
        // in the derivation's scope.
        let primary = ir
            .nodes
            .values()
            .find(|n| n.canonical_key.as_deref() == Some(derivation.key) && n.scope == scope)?;
        return key_label(primary, ir, derivation.key).map(|(label, _)| Route::Clone(label));
    }
    own_label.map(|label| Route::Hub(label.to_string()))
}

/// `W095`: a key some spoke requires from the hub that no *other* spoke maps.
/// Every transform into that spoke, except from itself, would then report
/// `REQUIRED_MISSING` for it — a requirement no source can meet.
pub fn check_required_routes(contracts: &BTreeMap<String, SpokeContract>) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (id, contract) in contracts {
        for route in &contract.required {
            let Some(label) = route.route.needs() else {
                continue;
            };
            let supplied = contracts
                .iter()
                .any(|(other, c)| other != id && c.keys.iter().any(|k| k.label == label));
            if !supplied && contracts.len() > 1 {
                diags.push(Diagnostic {
                    code: "W095".to_string(),
                    severity: Severity::Warning,
                    source_node: Some(format!("{id}::{}", route.node)),
                    message: format!(
                        "required write route `{label}` is mapped by no other spoke: every \
                         transform into `{id}` except from itself will report REQUIRED_MISSING"
                    ),
                    span: None,
                });
            }
        }
    }
    diags
}

/// Renders `contract` as a Rust `static` initializer expression of the
/// runtime contract type: `types` is the module path holding
/// `TransformationContract`, `KeyContract`, `RequiredRoute`, `Route`,
/// `Collapse` and `Selector` (the interfaces crate's `crate::contract`).
pub fn render_contract(contract: &SpokeContract, types: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{types}::TransformationContract {{");
    let _ = writeln!(out, "    spoke: {:?},", contract.spoke);
    out.push_str("    keys: &[\n");
    for k in &contract.keys {
        let scope = k
            .scope
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "        {types}::KeyContract {{ label: {:?}, key: {:?}, scope: &[{scope}], ty: {:?}, codec: {}, pinned: {} }},",
            k.label,
            k.key,
            k.ty,
            option_str(k.codec.as_deref()),
            option_str(k.pinned.as_deref()),
        );
    }
    out.push_str("    ],\n");
    out.push_str("    required: &[\n");
    for r in &contract.required {
        let route = match &r.route {
            Route::Hub(label) => format!("{types}::Route::Hub({label:?})"),
            Route::Constant(value) => format!("{types}::Route::Constant({value:?})"),
            Route::Clone(label) => format!("{types}::Route::Clone({label:?})"),
        };
        let _ = writeln!(
            out,
            "        {types}::RequiredRoute {{ node: {:?}, route: {route} }},",
            r.node
        );
    }
    out.push_str("    ],\n");
    out.push_str("    collapses: &[\n");
    for c in &contract.collapses {
        let _ = writeln!(
            out,
            "        {types}::Collapse {{ node: {:?}, label: {:?}, policy: {:?} }},",
            c.node, c.label, c.policy
        );
    }
    out.push_str("    ],\n");
    out.push_str("    selectors: &[\n");
    for s in &contract.selectors {
        let pairs = s
            .selector
            .iter()
            .map(|(k, v)| format!("({k:?}, {v:?})"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "        {types}::Selector {{ node: {:?}, element: {:?}, selector: &[{pairs}], single: {} }},",
            s.node, s.element, s.single
        );
    }
    out.push_str("    ],\n");
    out.push('}');
    out
}

/// `Some("…")` / `None` as Rust source.
fn option_str(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("Some({v:?})"),
        None => "None".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CodecTable;
    use crate::compile::{SpokeInput, compile};
    use crate::parse::{ParsedMapping, parse_mapping};

    fn mapping(id: &str, body: &str) -> ParsedMapping {
        parse_mapping(&format!(
            "[meta]\ndoc_format = \"{id}\"\nformat_version = \"1\"\nmapping_version = \"1\"\ncanonical_model = \"c:1\"\nroot = \"Doc\"\n[meta.namespaces]\n\"\" = \"urn:{id}\"\n{body}"
        ))
        .expect("parses")
    }

    fn contract_of(body: &str) -> SpokeContract {
        let m = mapping("a", body);
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&m),
        }];
        let out = compile(&spokes, &CodecTable::new());
        assert!(!out.has_errors(), "{:?}", out.diagnostics);
        spoke_contract("a:1", &out.irs["a"], &out.sources["a"])
    }

    const BODY: &str = r#"[Doc.ID]
        type = "identifier"
        canonical_key = "InvoiceNumber"
        required = true

        [Doc.Version]
        type = "identifier"
        canonical_key = "Version"
        constant = "2.1"
        required = true

        [Doc.Copy]
        type = "identifier"
        clone_of = "InvoiceNumber"
        required = true

        [Doc.Note]
        type = "string"
        canonical_key = "Note"
        multiple = "join"
        join_with = "\n"

        [Doc.Line]
        type = "collection"
        canonical_key = "Lines"
        required = true

        [Doc.Line.ID]
        type = "identifier"
        canonical_key = "LineId"

        [Doc.Line.Version]
        type = "identifier"
        clone_of = "$root.Version"
        required = true

        [Doc.Ref]
        type = "collection"
        canonical_key = "Refs"

        [Doc.Ref.ID]
        type = "identifier"
        canonical_key = "RefId"

        [Doc.Ref.TypeCode]
        type = "string"

        [Doc.Object]
        xml = "Ref"
        match = { "TypeCode" = "130" }

        [Doc.Object.ID]
        type = "identifier"
        canonical_key = "ObjectId""#;

    #[test]
    fn test_spoke_contract_keys_are_labelled_scoped_typed_and_pinned() {
        let c = contract_of(BODY);
        let labels: Vec<&str> = c.keys.iter().map(|k| k.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "InvoiceNumber",
                "Lines",
                "Lines/LineId",
                "Note",
                "ObjectId",
                "Refs",
                "Refs/RefId",
                "Version"
            ]
        );
        let line_id = &c.keys[2];
        assert_eq!(line_id.key, "LineId");
        assert_eq!(line_id.scope, ["Lines"]);
        assert_eq!(line_id.ty, "identifier");
        assert_eq!(c.keys[1].ty, "collection");
        assert_eq!(c.keys[7].pinned.as_deref(), Some("2.1"));
        assert_eq!(c.keys[0].pinned, None);
    }

    #[test]
    fn test_spoke_contract_required_routes() {
        let c = contract_of(BODY);
        let routes: Vec<(&str, &Route)> = c
            .required
            .iter()
            .map(|r| (r.node.as_str(), &r.route))
            .collect();
        assert_eq!(
            routes,
            [
                ("Doc.Copy", &Route::Clone("InvoiceNumber".into())),
                ("Doc.ID", &Route::Hub("InvoiceNumber".into())),
                ("Doc.Line", &Route::Hub("Lines".into())),
                ("Doc.Line.Version", &Route::Clone("Version".into())),
                ("Doc.Version", &Route::Constant("2.1".into())),
            ]
        );
    }

    #[test]
    fn test_spoke_contract_collapses_and_selectors() {
        let c = contract_of(BODY);
        assert_eq!(
            c.collapses,
            [Collapse {
                node: "Doc.Note".into(),
                label: "Note".into(),
                policy: "join".into()
            }]
        );
        assert_eq!(c.selectors.len(), 2);
        assert_eq!(c.selectors[0].node, "Doc.Object");
        assert_eq!(c.selectors[0].element, "Ref");
        assert_eq!(
            c.selectors[0].selector,
            [("type_code".to_string(), "130".to_string())]
        );
        assert!(c.selectors[0].single);
        assert_eq!(c.selectors[1].node, "Doc.Ref");
        assert!(c.selectors[1].selector.is_empty() && !c.selectors[1].single);
    }

    #[test]
    fn test_render_contract_is_valid_looking_rust() {
        let c = contract_of(BODY);
        let text = render_contract(&c, "crate::contract");
        assert!(text.starts_with("crate::contract::TransformationContract {"));
        assert!(text.contains(
            r#"crate::contract::KeyContract { label: "Lines/LineId", key: "LineId", scope: &["Lines"], ty: "identifier", codec: None, pinned: None },"#
        ));
        assert!(text.contains(
            r#"crate::contract::RequiredRoute { node: "Doc.Version", route: crate::contract::Route::Constant("2.1") },"#
        ));
        assert!(text.contains(
            r#"crate::contract::Selector { node: "Doc.Object", element: "Ref", selector: &[("type_code", "130")], single: true },"#
        ));
        assert!(text.ends_with('}'));
    }

    #[test]
    fn test_check_required_routes_warns_when_no_other_spoke_supplies_a_key() {
        let a = mapping(
            "a",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            required = true

            [Doc.Only]
            type = "identifier"
            canonical_key = "OnlyHere"
            required = true

            [Doc.Pinned]
            type = "identifier"
            canonical_key = "PinnedHere"
            constant = "x"
            required = true"#,
        );
        let b = mapping(
            "b",
            r#"[Doc.Number]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let spokes = [
            SpokeInput {
                id: "a".into(),
                chain: std::slice::from_ref(&a),
            },
            SpokeInput {
                id: "b".into(),
                chain: std::slice::from_ref(&b),
            },
        ];
        let out = compile(&spokes, &CodecTable::new());
        let w095: Vec<&Diagnostic> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == "W095")
            .collect();
        assert_eq!(w095.len(), 1, "{:?}", out.diagnostics);
        assert_eq!(w095[0].severity, Severity::Warning);
        assert_eq!(w095[0].source_node.as_deref(), Some("a::Doc.Only"));
        assert!(w095[0].message.contains("OnlyHere"));
        assert!(!out.has_errors());
    }

    #[test]
    fn test_check_required_routes_is_silent_for_a_single_spoke() {
        let a = mapping(
            "a",
            r#"[Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            required = true"#,
        );
        let spokes = [SpokeInput {
            id: "a".into(),
            chain: std::slice::from_ref(&a),
        }];
        let out = compile(&spokes, &CodecTable::new());
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }
}
