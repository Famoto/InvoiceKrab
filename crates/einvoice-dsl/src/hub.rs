//! Canonical hub derivation.
//!
//! KrabInvoice **derives** the canonical model: the hub is the union of every
//! `canonical_key` declared across every spoke mapping. The compiler enforces
//! **cross-spoke consistency** — a canonical key used by two spokes must agree
//! on type, collection-ness, and scope — so the emergent hub stays coherent.
//!
//! # Structure
//!
//! - [`CanonicalModel`] — the derived hub: canonical fields keyed by (scope, key).
//! - [`CanonicalField`] / [`CanonicalScope`] — one hub field and its scope.
//! - [`derive_hub`] — folds a set of [`MappingIr`]s into the model + diagnostics.
//!
//! # Behavior
//!
//! - A node's canonical scope is its enclosing collection's `canonical_key`
//!   (collection child keys are relative to the canonical collection item).
//! - A mapped child inside a collection whose own node has no `canonical_key`
//!   is an error (`E011`): there is no canonical item to attach to.
//! - A canonical key declared with a conflicting type/shape across spokes is an
//!   error (`E010`); the first declaration in deterministic order wins the slot.
//! - A canonical key declared twice within one spoke in the same scope is an
//!   error (`E013`): the read winner would otherwise fall out of node-id sort
//!   order — read priority must be spelled out with `fallbacks` instead.
//! - A name that would collide in the generated hub code — the same collection
//!   key in two scopes (duplicate `{key}Item` struct) or two keys collapsing to
//!   one snake_case field — is an error (`E012`).

use std::collections::BTreeMap;

use crate::error::{Diagnostic, Severity};
use crate::ir::MappingIr;
use crate::node::{Scope, SourceNode};
use crate::types::MappingType;

/// The scope of a canonical field.
///
/// A field is either at the invoice root or inside a *chain* of nested canonical
/// collections, named outermost-first by their canonical keys. A direct
/// invoice-line field is `Collection(["InvoiceLines"])`; a field of a collection
/// nested inside the line (e.g. a line-level allowance) is
/// `Collection(["InvoiceLines", "LineAllowances"])`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalScope {
    /// A top-level invoice field.
    Root,
    /// A field inside the collection chain named by these canonical keys
    /// (outermost first).
    Collection(Vec<String>),
}

impl CanonicalScope {
    /// The scope one level deeper: the item scope of a collection field named
    /// `key` that itself lives in `self`.
    pub fn child(&self, key: &str) -> CanonicalScope {
        let mut chain = match self {
            CanonicalScope::Root => Vec::new(),
            CanonicalScope::Collection(chain) => chain.clone(),
        };
        chain.push(key.to_string());
        CanonicalScope::Collection(chain)
    }
}

/// One derived canonical field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalField {
    /// The canonical key (target field name).
    pub key: String,
    /// Agreed value type.
    pub ty: MappingType,
    /// Whether the field is itself a canonical collection.
    pub is_collection: bool,
    /// The scope the field lives in.
    pub scope: CanonicalScope,
}

/// The derived canonical hub model (the union of spoke canonical keys).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanonicalModel {
    /// Canonical fields keyed by (scope, key) for deterministic order + lookup.
    pub fields: BTreeMap<(CanonicalScope, String), CanonicalField>,
}

impl CanonicalModel {
    /// Looks up a canonical field by scope and key.
    pub fn get(&self, scope: &CanonicalScope, key: &str) -> Option<&CanonicalField> {
        self.fields.get(&(scope.clone(), key.to_string()))
    }

    /// Whether a canonical key exists in the given scope.
    pub fn contains(&self, scope: &CanonicalScope, key: &str) -> bool {
        self.get(scope, key).is_some()
    }

    /// The number of distinct canonical fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the model has no fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// The canonical scope of a node: the chain of enclosing collections' canonical
/// keys (outermost first), or root. Returns `None` when any enclosing collection
/// has no `canonical_key` (there is no canonical item to attach to — `E011`).
pub fn canonical_scope_of(node: &SourceNode, mapping: &MappingIr) -> Option<CanonicalScope> {
    match &node.scope {
        Scope::Root => Some(CanonicalScope::Root),
        Scope::Collection(coll_id) => {
            // Walk the collection nesting inner-to-outer, collecting each
            // collection's canonical key; bail if any lacks one.
            let mut chain = Vec::new();
            let mut current = Some(coll_id.clone());
            while let Some(id) = current {
                let coll = mapping.nodes.get(&id)?;
                chain.push(coll.canonical_key.clone()?);
                current = match &coll.scope {
                    Scope::Collection(outer) => Some(outer.clone()),
                    Scope::Root => None,
                };
            }
            chain.reverse();
            Some(CanonicalScope::Collection(chain))
        }
    }
}

/// Derives the canonical hub from a set of mappings (one per spoke).
///
/// Returns the model plus any cross-spoke consistency diagnostics. Processing
/// is deterministic (mappings and nodes are iterated in sorted order), so the
/// "first declaration wins" tie-break is stable. Takes the mappings by reference
/// (any `IntoIterator` of `&MappingIr`), so callers need not clone their IRs.
pub fn derive_hub<'a>(
    mappings: impl IntoIterator<Item = &'a MappingIr>,
) -> (CanonicalModel, Vec<Diagnostic>) {
    let mut model = CanonicalModel::default();
    let mut diags = Vec::new();
    // Generated-code name collisions (E012): a collection key's item struct is
    // named `{key}Item` regardless of scope, and a scalar key's hub field is its
    // snake_case — either colliding would make the generated hub fail rustc with
    // a confusing duplicate-definition error, so catch both here.
    let mut collection_scopes: BTreeMap<String, CanonicalScope> = BTreeMap::new();
    let mut field_names: BTreeMap<(CanonicalScope, String), String> = BTreeMap::new();

    for mapping in mappings {
        // Within one spoke a canonical (scope, key) may be declared only once.
        // A second declaration would leave the read winner to node-id sort
        // order — ambiguous to the author, so it is an error (E013); read
        // priority is expressed explicitly via `fallbacks`.
        let mut declared: BTreeMap<(CanonicalScope, String), String> = BTreeMap::new();
        for node in mapping.nodes.values() {
            let Some(key) = node.canonical_key.clone() else {
                continue; // helper node: no canonical target.
            };

            // The canonical scope is the enclosing collection's canonical key.
            let Some(scope) = canonical_scope_of(node, mapping) else {
                let coll_id = match &node.scope {
                    Scope::Collection(c) => c,
                    Scope::Root => unreachable!("root scope always resolves"),
                };
                diags.push(Diagnostic {
                    code: "E011".to_string(),
                    severity: Severity::Error,
                    source_node: Some(node.id.to_string()),
                    message: format!(
                        "node `{}` maps `{key}` inside collection `{coll_id}`, \
                         but that collection has no canonical_key",
                        node.id
                    ),
                    span: None,
                });
                continue;
            };

            if let Some(first) = declared.get(&(scope.clone(), key.clone())) {
                diags.push(Diagnostic {
                    code: "E013".to_string(),
                    severity: Severity::Error,
                    source_node: Some(node.id.to_string()),
                    message: format!(
                        "canonical key `{key}` is mapped by both `{first}` and `{}` in this \
                         spoke — use `fallbacks` on one node to declare read priority, \
                         `clone_of` to mirror the value to a second path, or rename one key",
                        node.id
                    ),
                    span: None,
                });
                continue;
            }
            declared.insert((scope.clone(), key.clone()), node.id.to_string());

            let field = CanonicalField {
                key: key.clone(),
                ty: node.source_type,
                is_collection: node.is_collection(),
                scope: scope.clone(),
            };

            match model.fields.get(&(scope.clone(), key.clone())) {
                Some(existing)
                    if existing.ty != field.ty || existing.is_collection != field.is_collection =>
                {
                    diags.push(Diagnostic {
                        code: "E010".to_string(),
                        severity: Severity::Error,
                        source_node: Some(node.id.to_string()),
                        message: format!(
                            "canonical key `{key}` is declared as `{}`{} here but `{}`{} elsewhere",
                            field.ty,
                            if field.is_collection {
                                " collection"
                            } else {
                                ""
                            },
                            existing.ty,
                            if existing.is_collection {
                                " collection"
                            } else {
                                ""
                            },
                        ),
                        span: None,
                    });
                }
                Some(_) => {} // consistent re-declaration.
                None => {
                    if field.is_collection
                        && let Some(other) = collection_scopes.get(&key)
                        && *other != scope
                    {
                        diags.push(Diagnostic {
                            code: "E012".to_string(),
                            severity: Severity::Error,
                            source_node: Some(node.id.to_string()),
                            message: format!(
                                "canonical collection key `{key}` is used in two different \
                                 scopes; its generated `{key}Item` struct would be defined \
                                 twice — rename one of the keys"
                            ),
                            span: None,
                        });
                        continue;
                    }
                    let rust_name = crate::codegen::naming::snake_case(&key);
                    if let Some(existing_key) = field_names.get(&(scope.clone(), rust_name.clone()))
                        && *existing_key != key
                    {
                        diags.push(Diagnostic {
                            code: "E012".to_string(),
                            severity: Severity::Error,
                            source_node: Some(node.id.to_string()),
                            message: format!(
                                "canonical keys `{existing_key}` and `{key}` collapse to the \
                                 same generated hub field `{rust_name}` — rename one of them"
                            ),
                            span: None,
                        });
                        continue;
                    }
                    if field.is_collection {
                        collection_scopes.insert(key.clone(), scope.clone());
                    }
                    field_names.insert((scope.clone(), rust_name), key.clone());
                    model.fields.insert((scope, key), field);
                }
            }
        }
    }

    (model, diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::build_ir;
    use crate::parse::parse_mapping;

    fn ir(meta_model: &str, body: &str) -> MappingIr {
        let src = format!(
            r#"
            [meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            source_model = "{meta_model}"
            canonical_model = "c:1"
            {body}
        "#
        );
        let (ir, _source, diags) = build_ir(&[parse_mapping(&src).expect("parses")]);
        assert!(diags.is_empty(), "ir diags: {diags:?}");
        ir
    }

    #[test]
    fn test_single_spoke_root_field() {
        let m = ir(
            "a:1",
            r#"[Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let (model, diags) = derive_hub(&[m]);
        assert!(diags.is_empty());
        let f = model.get(&CanonicalScope::Root, "InvoiceNumber").unwrap();
        assert_eq!(f.ty, MappingType::Identifier);
        assert!(!f.is_collection);
    }

    #[test]
    fn test_helper_node_contributes_nothing() {
        let m = ir(
            "a:1",
            r#"[Invoice.UUID]
            type = "identifier""#,
        );
        let (model, _) = derive_hub(&[m]);
        assert!(model.is_empty());
    }

    #[test]
    fn test_two_spokes_consistent_key_merges() {
        let a = ir(
            "a:1",
            r#"[Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let b = ir(
            "b:1",
            r#"[Doc.Number]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let (model, diags) = derive_hub(&[a, b]);
        assert!(diags.is_empty(), "consistent keys must not conflict");
        assert_eq!(model.len(), 1);
    }

    #[test]
    fn test_two_spokes_conflicting_type_is_e010() {
        let a = ir(
            "a:1",
            r#"[Invoice.Total]
            type = "decimal"
            canonical_key = "PayableAmount""#,
        );
        let b = ir(
            "b:1",
            r#"[Doc.Total]
            type = "string"
            canonical_key = "PayableAmount""#,
        );
        let (_, diags) = derive_hub(&[a, b]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E010");
    }

    #[test]
    fn test_collection_child_scoped_to_collection_key() {
        let m = ir(
            "a:1",
            r#"[InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.ID]
            type = "identifier"
            canonical_key = "LineId""#,
        );
        let (model, diags) = derive_hub(&[m]);
        assert!(diags.is_empty());
        assert!(model.contains(&CanonicalScope::Root, "InvoiceLines"));
        assert!(model.contains(
            &CanonicalScope::Collection(vec!["InvoiceLines".to_string()]),
            "LineId"
        ));
        // The same key name at root would be a different field (different scope).
        assert!(!model.contains(&CanonicalScope::Root, "LineId"));
    }

    #[test]
    fn test_nested_collection_child_scoped_to_full_chain() {
        // A collection nested inside the invoice-line collection: its child's
        // canonical scope is the full chain, outermost first.
        let m = ir(
            "a:1",
            r#"[InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.AllowanceCharge]
            type = "collection"
            canonical_key = "LineAllowances"

            [InvoiceLine.AllowanceCharge.Amount]
            type = "decimal"
            canonical_key = "LineAllowanceAmount""#,
        );
        let (model, diags) = derive_hub(&[m]);
        assert!(diags.is_empty(), "{diags:?}");
        // The nested collection is itself a field of the invoice-line item.
        let lines = CanonicalScope::Collection(vec!["InvoiceLines".to_string()]);
        let nested = model.get(&lines, "LineAllowances").expect("nested coll");
        assert!(nested.is_collection);
        // Its child lives two levels deep.
        assert!(model.contains(
            &CanonicalScope::Collection(vec![
                "InvoiceLines".to_string(),
                "LineAllowances".to_string()
            ]),
            "LineAllowanceAmount"
        ));
    }

    #[test]
    fn test_collection_key_in_two_scopes_is_e012() {
        // The same canonical collection key at root in one spoke and nested in
        // another would generate two `LinesItem` structs — a rustc duplicate-
        // definition error in the generated hub. Caught here instead.
        let a = ir(
            "a:1",
            r#"[Line]
            type = "collection"
            canonical_key = "Lines""#,
        );
        let b = ir(
            "b:1",
            r#"[Group]
            type = "collection"
            canonical_key = "Groups"

            [Group.Line]
            type = "collection"
            canonical_key = "Lines""#,
        );
        let (_, diags) = derive_hub(&[a, b]);
        assert!(diags.iter().any(|d| d.code == "E012"), "{diags:?}");
    }

    #[test]
    fn test_two_keys_same_rust_field_name_is_e012() {
        // `Foo_bar` and `FooBar` are different canonical keys but collapse to
        // the same generated `foo_bar` hub field.
        let a = ir(
            "a:1",
            r#"[Invoice.A]
            type = "string"
            canonical_key = "Foo_bar""#,
        );
        let b = ir(
            "b:1",
            r#"[Invoice.B]
            type = "string"
            canonical_key = "FooBar""#,
        );
        let (_, diags) = derive_hub(&[a, b]);
        assert!(diags.iter().any(|d| d.code == "E012"), "{diags:?}");
    }

    #[test]
    fn test_same_spoke_duplicate_key_is_e013() {
        // Two nodes in one spoke mapping the same canonical key in the same
        // scope: the read priority would be decided by node-id sort order —
        // ambiguous to the author, so it is an error. `fallbacks` is the
        // explicit way to express priority.
        let m = ir(
            "a:1",
            r#"[Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"

            [Invoice.Ref]
            type = "identifier"
            canonical_key = "InvoiceNumber""#,
        );
        let (_, diags) = derive_hub(&[m]);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "E013");
        assert!(
            diags[0].message.contains("Invoice.ID") && diags[0].message.contains("Invoice.Ref"),
            "message must name both nodes: {}",
            diags[0].message
        );
    }

    #[test]
    fn test_same_spoke_same_key_different_scopes_is_ok() {
        // The same key at root and inside a collection are different canonical
        // fields (different scope) — not a duplicate.
        let m = ir(
            "a:1",
            r#"[Invoice.Note]
            type = "string"
            canonical_key = "Note"

            [InvoiceLine]
            type = "collection"
            canonical_key = "InvoiceLines"

            [InvoiceLine.Note]
            type = "string"
            canonical_key = "Note""#,
        );
        let (model, diags) = derive_hub(&[m]);
        assert!(diags.is_empty(), "{diags:?}");
        assert!(model.contains(&CanonicalScope::Root, "Note"));
        assert!(model.contains(
            &CanonicalScope::Collection(vec!["InvoiceLines".to_string()]),
            "Note"
        ));
    }

    #[test]
    fn test_mapped_child_under_unmapped_collection_is_e011() {
        let m = ir(
            "a:1",
            r#"[Lines]
            type = "collection"

            [Lines.ID]
            type = "identifier"
            canonical_key = "LineId""#,
        );
        let (_, diags) = derive_hub(&[m]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E011");
    }
}
