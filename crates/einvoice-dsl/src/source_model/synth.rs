//! Source-model synthesis: build the struct tree and per-node source paths from
//! the mapping nodes.
//!
//! A node's id mirrors the XML element tree, so both the typed source structs and
//! every node's `source_path` are derived here from the node ids, their
//! `type`/`required`/`xml`, and `[meta].root` — no separate `[source]` tree.
//! Codegen then emits the source structs from the same [`SourceModelMeta`].

use std::collections::BTreeMap;

use crate::error::{Diagnostic, Severity};
use crate::node::{NodeId, RawNode, Scope};
use crate::types::MappingType;

use super::meta::{FieldMeta, FieldType, SourceModelMeta, StructMeta};

/// Synthesizes the typed source model from the mapping nodes.
///
/// A node's id mirrors the XML element tree (its segments are XML element local
/// names), so the struct tree, the `Option`/`Vec` wrappers, the serde XML
/// renames, and each node's `source_path` can all be derived without a separate
/// `[source]` tree. Returns the model, a `NodeId → source_path` map (the dotted
/// snake_case Rust field path relative to the node's scope struct), and any
/// diagnostics.
///
/// Rules:
/// - The node's scope is its nearest enclosing `collection` ancestor, else root
///   (matching [`crate::resolve`]). Each scope has a root struct: the root scope's
///   is `root`; a collection scope's is the collection's item struct.
/// - A node's element path within its scope is its id minus the scope prefix.
/// - A leaf with descendant nodes is a *valued container* (a struct with a
///   `$text` `value` field plus its descendants' fields); a leaf without
///   descendants is a scalar field; a `collection` node is a `Vec<Item>` field.
/// - Every scalar leaf is `Option<String>` (or `Vec<String>` with `multiple`);
///   `required` is enforced by the generated reader/writer as a
///   `REQUIRED_MISSING` diagnostic, never by failing deserialization.
pub fn synthesize_source_model(
    active: &BTreeMap<NodeId, RawNode>,
    root: &str,
    model_id: &str,
) -> (SourceModelMeta, BTreeMap<NodeId, String>, Vec<Diagnostic>) {
    let mut structs: BTreeMap<String, StructMeta> = BTreeMap::new();
    structs.insert(root.to_string(), StructMeta::default());
    let mut source_paths: BTreeMap<NodeId, String> = BTreeMap::new();
    let mut diags: Vec<Diagnostic> = Vec::new();

    // Collection nodes define scopes (and item structs). Build the lookup first
    // so every node's scope and base struct can be computed.
    let collections: BTreeMap<NodeId, MappingType> = active
        .iter()
        .filter_map(|(id, n)| {
            n.ty.filter(|t| *t == MappingType::Collection)
                .map(|t| (id.clone(), t))
        })
        .collect();

    for (id, node) in active {
        let Some(ty) = node.ty else {
            // Untyped tables are containers inferred from descendant ids, not
            // standalone nodes; nothing to place. (A disabled-only override has
            // already been removed before synthesis.)
            continue;
        };
        let scope = id.nearest_collection_scope(|a| collections.contains_key(a));
        let base = scope_struct(&scope, root, &collections);
        let segments = element_path(id, &scope, root);
        if segments.is_empty() {
            diags.push(synth_err(
                id,
                format!("node `{id}` has no element path under its scope"),
            ));
            continue;
        }

        match insert_node(&mut structs, &base, &segments, node, ty, active, id) {
            Ok(path) => {
                source_paths.insert(id.clone(), path);
            }
            Err(msg) => diags.push(synth_err(id, msg)),
        }
    }

    (
        SourceModelMeta {
            model_id: model_id.to_string(),
            root: root.to_string(),
            structs,
        },
        source_paths,
        diags,
    )
}

/// The struct a scope's nodes are placed into: the model root for root scope, or
/// the collection's item struct for a collection scope.
fn scope_struct(scope: &Scope, root: &str, _collections: &BTreeMap<NodeId, MappingType>) -> String {
    match scope {
        Scope::Root => root.to_string(),
        Scope::Collection(coll) => item_struct_name(coll),
    }
}

/// The item-struct name for a collection node: the CamelCase of its last id
/// segment (the repeated element's local name).
fn item_struct_name(coll: &NodeId) -> String {
    camel_case(coll.segments().last().unwrap_or(""))
}

/// A node's XML element path within its scope: its id segments with the scope
/// prefix removed. Root scope drops a leading `root` segment if present (so
/// `Invoice.ID` → `[ID]`, while a bare collection id like `InvoiceLine` keeps all
/// segments); a collection scope drops the collection node's id prefix.
fn element_path(id: &NodeId, scope: &Scope, root: &str) -> Vec<String> {
    let segs: Vec<String> = id.segments().map(str::to_string).collect();
    match scope {
        Scope::Root => {
            if segs.first().map(String::as_str) == Some(root) {
                segs[1..].to_vec()
            } else {
                segs
            }
        }
        Scope::Collection(coll) => {
            let skip = coll.segments().count();
            segs[skip..].to_vec()
        }
    }
}

/// Inserts one node's element path into the struct table, creating interior
/// structs as needed, and returns the node's `source_path` (dotted snake field
/// path relative to its scope struct).
fn insert_node(
    structs: &mut BTreeMap<String, StructMeta>,
    base: &str,
    segments: &[String],
    node: &RawNode,
    ty: MappingType,
    active: &BTreeMap<NodeId, RawNode>,
    id: &NodeId,
) -> Result<String, String> {
    // Descend/create interior structs for all but the final segment.
    let mut current = base.to_string();
    let mut path_parts: Vec<String> = Vec::new();
    for seg in &segments[..segments.len() - 1] {
        let field = snake_case(seg);
        let struct_name = camel_case(seg);
        upsert_field(
            structs,
            &current,
            &field,
            FieldMeta {
                optional: false,
                repeated: false,
                ty: FieldType::Struct(struct_name.clone()),
                xml: Some(seg.clone()),
            },
        )?;
        structs.entry(struct_name.clone()).or_default();
        path_parts.push(field);
        current = struct_name;
    }

    let last = &segments[segments.len() - 1];
    // Leaves are always `Option<String>` (with a serde `default`), whatever
    // `required` says: a document missing the element must still *parse*, so
    // the generated reader/writer can report REQUIRED_MISSING as a structured
    // diagnostic instead of the parse failing with a raw serde error.
    let optional = true;

    // `multiple` opts a plain scalar element leaf into a `Vec<String>` source
    // field. Every other node shape either cannot repeat in XML (attributes,
    // element text) or already repeats structurally (collections).
    let multi = node.multiple.is_some();

    // Collection node: a repeated struct field; children populate the item struct.
    if ty == MappingType::Collection {
        if multi {
            return Err(
                "`multiple` is not valid on a collection node (a collection already repeats)"
                    .to_string(),
            );
        }
        let field = snake_case(last);
        let item = item_struct_name(id);
        let rename = node.xml.clone().unwrap_or_else(|| last.clone());
        upsert_field(
            structs,
            &current,
            &field,
            FieldMeta {
                optional: false,
                repeated: true,
                ty: FieldType::Struct(item.clone()),
                xml: Some(rename),
            },
        )?;
        structs.entry(item).or_default();
        path_parts.push(field);
        return Ok(path_parts.join("."));
    }

    // Attribute leaf (`xml = "@..."`): a scalar field on the current struct.
    if let Some(xml) = node.xml.as_deref()
        && xml.starts_with('@')
    {
        if multi {
            return Err(
                "`multiple` is not valid on an attribute leaf (an XML attribute cannot repeat)"
                    .to_string(),
            );
        }
        let field = snake_case(last);
        upsert_field(
            structs,
            &current,
            &field,
            FieldMeta {
                optional,
                repeated: false,
                ty: FieldType::Scalar,
                xml: Some(xml.to_string()),
            },
        )?;
        path_parts.push(field);
        return Ok(path_parts.join("."));
    }

    // Element text override (`xml = "$text"`): a value field on the current struct.
    if node.xml.as_deref() == Some("$text") {
        if multi {
            return Err(
                "`multiple` is not valid on a `$text` leaf (element text cannot repeat)"
                    .to_string(),
            );
        }
        upsert_field(
            structs,
            &current,
            "value",
            FieldMeta {
                optional,
                repeated: false,
                ty: FieldType::Scalar,
                xml: Some("$text".to_string()),
            },
        )?;
        path_parts.push("value".to_string());
        return Ok(path_parts.join("."));
    }

    // A typed element with descendant nodes is a *valued container*: its own
    // value is the element text, carried by a `$text` `value` field inside a
    // struct that also holds its descendants (e.g. an `@currencyID` attribute).
    if has_descendant(active, id) {
        if multi {
            return Err(
                "`multiple` is not valid on a valued container (model the repetition as a collection instead)"
                    .to_string(),
            );
        }
        let field = snake_case(last);
        let struct_name = camel_case(last);
        let rename = node.xml.clone().unwrap_or_else(|| last.clone());
        upsert_field(
            structs,
            &current,
            &field,
            FieldMeta {
                optional: false,
                repeated: false,
                ty: FieldType::Struct(struct_name.clone()),
                xml: Some(rename),
            },
        )?;
        structs.entry(struct_name.clone()).or_default();
        upsert_field(
            structs,
            &struct_name,
            "value",
            FieldMeta {
                optional,
                repeated: false,
                ty: FieldType::Scalar,
                xml: Some("$text".to_string()),
            },
        )?;
        path_parts.push(field);
        path_parts.push("value".to_string());
        return Ok(path_parts.join("."));
    }

    // Plain scalar leaf: a field on the current struct, optionally renamed.
    // With `multiple` declared the field is `Vec<String>` (repeated, never
    // `Option`-wrapped) so repeated source elements parse instead of failing.
    let field = snake_case(last);
    let rename = node.xml.clone().unwrap_or_else(|| last.clone());
    upsert_field(
        structs,
        &current,
        &field,
        FieldMeta {
            optional: optional && !multi,
            repeated: multi,
            ty: FieldType::Scalar,
            xml: Some(rename),
        },
    )?;
    path_parts.push(field);
    Ok(path_parts.join("."))
}

/// Whether any active node has `id` as a strict id prefix (i.e. `id` is a parent
/// element of another node).
///
/// `active` is sorted by id, and every descendant shares the `"{id}."` prefix, so
/// the descendants form one contiguous range. The first key at or after that
/// prefix is a descendant iff any exists — an `O(log n)` range probe rather than
/// an `O(n)` scan per node.
fn has_descendant(active: &BTreeMap<NodeId, RawNode>, id: &NodeId) -> bool {
    let prefix = format!("{id}.");
    active
        .range(NodeId::new(prefix.clone())..)
        .next()
        .is_some_and(|(other, _)| other.as_str().starts_with(&prefix))
}

/// Inserts `field` into struct `struct_name`, creating the struct if absent.
/// Re-inserting an identical field is fine (two nodes contributing to the same
/// struct); a conflicting redefinition is an error (E024).
fn upsert_field(
    structs: &mut BTreeMap<String, StructMeta>,
    struct_name: &str,
    field: &str,
    meta: FieldMeta,
) -> Result<(), String> {
    let entry = structs.entry(struct_name.to_string()).or_default();
    match entry.fields.get(field) {
        Some(existing) if *existing != meta => Err(format!(
            "synthesized field `{struct_name}.{field}` is defined two incompatible ways"
        )),
        _ => {
            entry.fields.insert(field.to_string(), meta);
            Ok(())
        }
    }
}

/// An `E024` synthesis diagnostic for `id`.
fn synth_err(id: &NodeId, message: String) -> Diagnostic {
    Diagnostic {
        code: "E024".to_string(),
        severity: Severity::Error,
        source_node: Some(id.to_string()),
        message,
        span: None,
    }
}

/// Converts a `snake_case`/`mixed` name to `CamelCase` (e.g. `legal_monetary_total`
/// → `LegalMonetaryTotal`, `PayableAmount` → `PayableAmount`).
fn camel_case(s: &str) -> String {
    s.split('_')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Converts an XML element/attribute local name to a `snake_case` Rust field name
/// (e.g. `IssueDate` → `issue_date`, `currencyID` → `currency_id`).
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            if i != 0 && (prev_lower || next_lower) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::resolve::resolve_path;
    use super::FieldType::{Scalar, Struct};
    use super::*;

    #[test]
    fn test_snake_case_xml_names() {
        assert_eq!(snake_case("ID"), "id");
        assert_eq!(snake_case("IssueDate"), "issue_date");
        assert_eq!(snake_case("currencyID"), "currency_id");
        assert_eq!(snake_case("UUID"), "uuid");
        assert_eq!(snake_case("InvoicedQuantity"), "invoiced_quantity");
    }

    fn raw(toml_src: &str) -> RawNode {
        toml::from_str(toml_src).expect("raw node parses")
    }

    fn nodes(pairs: &[(&str, &str)]) -> BTreeMap<NodeId, RawNode> {
        pairs
            .iter()
            .map(|(id, body)| (NodeId::new(*id), raw(body)))
            .collect()
    }

    fn synth(pairs: &[(&str, &str)]) -> (SourceModelMeta, BTreeMap<NodeId, String>) {
        let (model, paths, diags) = synthesize_source_model(&nodes(pairs), "Invoice", "ubl:2.1");
        assert!(diags.is_empty(), "unexpected synth diagnostics: {diags:?}");
        (model, paths)
    }

    #[test]
    fn test_synth_scalar_leaf_is_option_even_when_required() {
        // `required` is a reader/writer diagnostic, not a parse constraint: a
        // document missing the element must still deserialize.
        let (model, paths) = synth(&[
            (
                "Invoice.ID",
                r#"type = "identifier"
            required = true"#,
            ),
            ("Invoice.IssueDate", r#"type = "date""#),
        ]);
        let inv = &model.structs["Invoice"];
        assert_eq!(inv.fields["id"].ty, Scalar);
        assert!(inv.fields["id"].optional, "required leaf is still Option");
        assert_eq!(inv.fields["id"].xml.as_deref(), Some("ID"));
        assert!(inv.fields["issue_date"].optional);
        assert_eq!(paths[&NodeId::new("Invoice.ID")], "id");
        assert_eq!(paths[&NodeId::new("Invoice.IssueDate")], "issue_date");
    }

    #[test]
    fn test_synth_valued_container_with_attribute() {
        // PayableAmount carries a decimal text value AND a currencyID attribute.
        let (model, paths) = synth(&[
            (
                "Invoice.LegalMonetaryTotal.PayableAmount",
                r#"type = "decimal"
                required = true"#,
            ),
            (
                "Invoice.LegalMonetaryTotal.PayableAmount.currencyID",
                r#"xml = "@currencyID"
                type = "currency""#,
            ),
        ]);
        // Invoice.legal_monetary_total: LegalMonetaryTotal struct.
        assert_eq!(
            model.structs["Invoice"].fields["legal_monetary_total"].ty,
            Struct("LegalMonetaryTotal".into())
        );
        // LegalMonetaryTotal.payable_amount: PayableAmount struct.
        assert_eq!(
            model.structs["LegalMonetaryTotal"].fields["payable_amount"].ty,
            Struct("PayableAmount".into())
        );
        // PayableAmount has a $text value field and the currencyID attribute.
        let pa = &model.structs["PayableAmount"];
        assert_eq!(pa.fields["value"].xml.as_deref(), Some("$text"));
        assert!(pa.fields["value"].optional, "value leaf is always Option");
        assert_eq!(pa.fields["currency_id"].xml.as_deref(), Some("@currencyID"));
        assert!(pa.fields["currency_id"].optional);
        assert_eq!(
            paths[&NodeId::new("Invoice.LegalMonetaryTotal.PayableAmount")],
            "legal_monetary_total.payable_amount.value"
        );
        assert_eq!(
            paths[&NodeId::new("Invoice.LegalMonetaryTotal.PayableAmount.currencyID")],
            "legal_monetary_total.payable_amount.currency_id"
        );
    }

    #[test]
    fn test_synth_collection_and_item_children() {
        let (model, paths) = synth(&[
            (
                "InvoiceLine",
                r#"type = "collection"
                canonical_key = "InvoiceLines""#,
            ),
            ("InvoiceLine.ID", r#"type = "identifier""#),
            ("InvoiceLine.Item.Name", r#"type = "string""#),
        ]);
        // Root carries a Vec<InvoiceLine>.
        let coll = &model.structs["Invoice"].fields["invoice_line"];
        assert!(coll.repeated);
        assert_eq!(coll.ty, Struct("InvoiceLine".into()));
        assert_eq!(coll.xml.as_deref(), Some("InvoiceLine"));
        // Item children resolve against the item struct.
        assert_eq!(model.structs["InvoiceLine"].fields["id"].ty, Scalar);
        assert_eq!(
            model.structs["InvoiceLine"].fields["item"].ty,
            Struct("Item".into())
        );
        assert_eq!(model.structs["Item"].fields["name"].ty, Scalar);
        assert_eq!(paths[&NodeId::new("InvoiceLine")], "invoice_line");
        assert_eq!(paths[&NodeId::new("InvoiceLine.ID")], "id");
        assert_eq!(paths[&NodeId::new("InvoiceLine.Item.Name")], "item.name");
    }

    #[test]
    fn test_synth_resolves_against_itself() {
        // The synthesized model is consistent: every node's source_path resolves.
        let (model, paths) = synth(&[
            ("Invoice.ID", r#"type = "identifier""#),
            (
                "Invoice.LegalMonetaryTotal.PayableAmount",
                r#"type = "decimal""#,
            ),
        ]);
        for path in paths.values() {
            assert!(
                resolve_path(&model, path).is_ok(),
                "synthesized path `{path}` must resolve"
            );
        }
    }

    #[test]
    fn test_synth_is_deterministic() {
        let pairs: &[(&str, &str)] = &[
            ("Invoice.ID", r#"type = "identifier""#),
            ("Invoice.IssueDate", r#"type = "date""#),
            ("InvoiceLine", r#"type = "collection""#),
            ("InvoiceLine.ID", r#"type = "identifier""#),
        ];
        let a = synthesize_source_model(&nodes(pairs), "Invoice", "ubl:2.1");
        let b = synthesize_source_model(&nodes(pairs), "Invoice", "ubl:2.1");
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn test_synth_multiple_leaf_is_repeated_vec() {
        let (model, paths) = synth(&[(
            "Invoice.Note",
            r#"type = "string"
            multiple = "join"
            join_with = "\n""#,
        )]);
        let note = &model.structs["Invoice"].fields["note"];
        assert!(note.repeated, "multiple leaf must synthesize Vec<String>");
        assert!(!note.optional, "a Vec is never Option-wrapped");
        assert_eq!(note.ty, Scalar);
        assert_eq!(paths[&NodeId::new("Invoice.Note")], "note");
    }

    #[test]
    fn test_synth_multiple_on_attribute_is_error() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[(
                "Invoice.Amount.currencyID",
                r#"xml = "@currencyID"
                type = "currency"
                multiple = "first""#,
            )]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E024" && d.message.contains("attribute")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_multiple_on_collection_is_error() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[(
                "InvoiceLine",
                r#"type = "collection"
                multiple = "first""#,
            )]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E024" && d.message.contains("collection")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_multiple_on_valued_container_is_error() {
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[
                (
                    "Invoice.Amount",
                    r#"type = "decimal"
                    multiple = "first""#,
                ),
                (
                    "Invoice.Amount.currencyID",
                    r#"xml = "@currencyID"
                    type = "currency""#,
                ),
            ]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == "E024" && d.message.contains("valued container")),
            "{diags:?}"
        );
    }

    #[test]
    fn test_synth_conflicting_field_is_e024() {
        // Two element names collapse to the same snake field but bind different
        // XML names (`ID` vs `Id`): the synthesized `id` field is ambiguous.
        let (_, _, diags) = synthesize_source_model(
            &nodes(&[
                ("Invoice.ID", r#"type = "identifier""#),
                ("Invoice.Id", r#"type = "string""#),
            ]),
            "Invoice",
            "ubl:2.1",
        );
        assert!(diags.iter().any(|d| d.code == "E024"), "{diags:?}");
    }
}
