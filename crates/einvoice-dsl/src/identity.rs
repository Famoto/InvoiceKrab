//! Build-time checks of the spokes' document identities.
//!
//! A document is read by a spoke only when its root element's namespace URI
//! and local name match the spoke's (`[meta.namespaces]` at `root_ns`, and
//! `root`) and it declares what `[meta.identity]` demands
//! ([`IdentityMeta`](crate::meta::IdentityMeta)). Auto-detection picks the one
//! spoke a document satisfies, so the identities must be well formed and must
//! tell apart every two spokes that share a root:
//!
//! - `E120` — a malformed `[meta.identity]`: `profile` without `profiles` (or
//!   the reverse), `version` without `versions` (or the reverse), an attribute
//!   with no accepted value, a path segment or attribute that is no XML name,
//!   or an accepted value that is empty or padded with whitespace (document
//!   text is trimmed before it is compared, so it could never match).
//! - `E121` — two emitted spokes share a root (namespace URI and local name)
//!   but their identities do not exclude each other: both must declare the
//!   same `profile` element with disjoint `profiles`, so no document is read
//!   by both.

use std::collections::BTreeMap;

use crate::error::{Diagnostic, Severity};
use crate::ident::is_xml_name;
use crate::ir::MappingIr;
use crate::meta::IdentityMeta;

/// Checks every spoke's `[meta.identity]` (`E120`) and that the spokes sharing
/// a root tell each other apart (`E121`). `irs` are the emitted spokes, keyed
/// by spoke id, with their effective (inherited) meta.
pub fn check_identities(irs: &BTreeMap<String, MappingIr>) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (spoke, ir) in irs {
        if let Some(identity) = &ir.meta.identity {
            for problem in malformed(identity) {
                diags.push(error("E120", spoke, problem));
            }
        }
    }

    let spokes: Vec<(&String, &MappingIr)> = irs.iter().collect();
    for (i, (a, ia)) in spokes.iter().enumerate() {
        for (b, ib) in &spokes[i + 1..] {
            let root = |ir: &MappingIr| {
                (
                    ir.meta.root_namespace().map(str::to_string),
                    ir.meta.root.clone().unwrap_or_else(|| "Root".to_string()),
                )
            };
            let (ns, local) = root(ia);
            if (ns.clone(), local.clone()) != root(ib) {
                continue;
            }
            if let Some(why) = overlap(ia.meta.identity.as_ref(), ib.meta.identity.as_ref()) {
                diags.push(error(
                    "E121",
                    a,
                    format!(
                        "shares the root <{local}> (namespace {}) with `{b}`, but {why}: a \
                         document could be read as either. Declare the same \
                         `[meta.identity].profile` on both, with disjoint `profiles`",
                        ns.as_deref().unwrap_or("none")
                    ),
                ));
            }
        }
    }
    diags
}

/// The problems of one `[meta.identity]` table (`E120`).
fn malformed(identity: &IdentityMeta) -> Vec<String> {
    let mut problems = Vec::new();
    for (field, path, values, list) in [
        ("profile", &identity.profile, &identity.profiles, "profiles"),
        ("version", &identity.version, &identity.versions, "versions"),
    ] {
        match path {
            Some(path) => {
                if values.is_empty() {
                    problems.push(format!("`{field}` is set but `{list}` is empty"));
                }
                if path.split('.').any(|seg| !is_xml_name(seg)) {
                    problems.push(format!(
                        "`{field}` path `{path}` is not a dotted path of XML names"
                    ));
                }
            }
            None if !values.is_empty() => {
                problems.push(format!("`{list}` is set but `{field}` names no element"));
            }
            None => {}
        }
        problems.extend(bad_values(list, values));
    }
    for (name, values) in &identity.attributes {
        if !is_xml_name(name) {
            problems.push(format!("attribute `{name}` is not an XML name"));
        }
        if values.is_empty() {
            problems.push(format!("attribute `{name}` accepts no value"));
        }
        problems.extend(bad_values(&format!("attributes.{name}"), values));
    }
    problems
}

/// Accepted values that could never match trimmed document text.
fn bad_values(list: &str, values: &[String]) -> Vec<String> {
    values
        .iter()
        .filter(|v| v.is_empty() || v.trim() != v.as_str())
        .map(|v| format!("`{list}` value {v:?} is empty or padded with whitespace"))
        .collect()
}

/// Why two identities on one root do not exclude each other, or `None` when
/// they do (same `profile` element, disjoint `profiles`).
fn overlap(a: Option<&IdentityMeta>, b: Option<&IdentityMeta>) -> Option<String> {
    let (Some(a), Some(b)) = (a, b) else {
        return Some("not both declare a `[meta.identity]`".to_string());
    };
    let (Some(pa), Some(pb)) = (&a.profile, &b.profile) else {
        return Some("not both declare a `profile`".to_string());
    };
    if pa != pb {
        return Some(format!(
            "their `profile` elements differ (`{pa}` vs `{pb}`)"
        ));
    }
    let shared: Vec<&String> = a
        .profiles
        .iter()
        .filter(|p| b.profiles.contains(p))
        .collect();
    if shared.is_empty() {
        None
    } else {
        Some(format!("both accept the profile {shared:?}"))
    }
}

fn error(code: &str, spoke: &str, message: String) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity: Severity::Error,
        source_node: Some(spoke.to_string()),
        message,
        span: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::build_ir;
    use crate::parse::parse_mapping;

    fn ir(meta_extra: &str) -> MappingIr {
        let src = format!(
            r#"
            [meta]
            doc_format = "f"
            format_version = "1"
            mapping_version = "1"
            canonical_model = "c:1"
            root = "Invoice"
            [meta.namespaces]
            "" = "urn:inv"
            {meta_extra}

            [Invoice.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
            "#
        );
        build_ir(&[parse_mapping(&src).expect("parses")]).0
    }

    fn codes(irs: &[(&str, MappingIr)]) -> Vec<String> {
        let map = irs
            .iter()
            .map(|(id, ir)| (id.to_string(), ir.clone()))
            .collect();
        check_identities(&map).into_iter().map(|d| d.code).collect()
    }

    const BASE: &str = r#"
        [meta.identity]
        profile = "CustomizationID"
        profiles = ["urn:base"]"#;

    #[test]
    fn test_disjoint_profiles_on_one_root_are_clean() {
        let cius = r#"
            [meta.identity]
            profile = "CustomizationID"
            profiles = ["urn:cius"]"#;
        assert!(codes(&[("a", ir(BASE)), ("b", ir(cius))]).is_empty());
    }

    #[test]
    fn test_overlapping_profiles_on_one_root_are_e121() {
        let cius = r#"
            [meta.identity]
            profile = "CustomizationID"
            profiles = ["urn:cius", "urn:base"]"#;
        assert_eq!(codes(&[("a", ir(BASE)), ("b", ir(cius))]), ["E121"]);
    }

    #[test]
    fn test_missing_identity_on_shared_root_is_e121() {
        assert_eq!(codes(&[("a", ir(BASE)), ("b", ir(""))]), ["E121"]);
    }

    #[test]
    fn test_different_root_namespace_needs_no_identity() {
        let mut other = ir("");
        other
            .meta
            .namespaces
            .as_mut()
            .unwrap()
            .insert(String::new(), "urn:other".into());
        assert!(codes(&[("a", ir("")), ("b", other)]).is_empty());
    }

    #[test]
    fn test_malformed_identity_is_e120() {
        let bad = r#"
            [meta.identity]
            profiles = [" urn:padded"]
            [meta.identity.attributes]
            versione = []"#;
        // profiles without profile, a padded value, an attribute with no value.
        assert_eq!(codes(&[("a", ir(bad))]), ["E120", "E120", "E120"]);
    }
}
