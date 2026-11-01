//! Alternatives: one value, several possible elements, chosen by `when`.
//!
//! Scalar nodes of one scope that share a `canonical_key`, where any of them
//! has a `when`, are the *alternatives* of that value (a single node with a
//! `when` is a set of one). On write the first in file order whose `when`
//! holds writes the value; the one without `when` (at most one) is the
//! default; a value none takes is `NO_TARGET`. On read the first in file order
//! that has a value supplies it (a later one that also has one is
//! `MULTIPLE_SOURCES`), and its `when` values are filled in where the hub has
//! none (a list: its first). A `when` on a constant writes the constant only
//! while it holds.
//!
//! `when` entries name hub keys of the node's own scope and take the
//! [`Condition`] forms of `match` (`$` references aside). A `placeholder` is
//! written when a node writes nothing, and reads back as no value.
//!
//! Build-time checks: `E130` a `when` entry names no scalar key of the scope,
//! compares a non-text key to a value, or is a reference; `E132` a misplaced
//! `when`, two defaults, a `when` that repeats an earlier one, or `required` /
//! `default` on one of several alternatives; `W140` alternatives without a
//! default (unless `[meta.defaults] no_target = "warn"` drops a value none
//! takes, with a warning); `E134` a misplaced `placeholder` or one that is no
//! literal of the node's type.

use std::collections::BTreeMap;

use crate::cond::Condition;
use crate::error::{Diagnostic, Severity};
use crate::hub::{CanonicalModel, canonical_scope_of};
use crate::ir::MappingIr;
use crate::node::{Scope, SourceNode};
use crate::types::MappingType;
use crate::validate::constant_literal_error;

/// Whether `node` can be an alternative: a keyed scalar of its own scope.
fn keyed_scalar(node: &SourceNode) -> bool {
    node.canonical_key.is_some()
        && !node.is_collection()
        && node.lift.is_none()
        && node.clone_of.is_none()
}

/// The alternative sets among `nodes` (one scope's scalars): per shared key
/// with any `when`, the nodes in file order; sets ordered by their first node.
pub fn groups<'a>(nodes: impl IntoIterator<Item = &'a SourceNode>) -> Vec<Vec<&'a SourceNode>> {
    let mut by_key: BTreeMap<&str, Vec<&SourceNode>> = BTreeMap::new();
    for node in nodes.into_iter().filter(|n| keyed_scalar(n)) {
        by_key
            .entry(node.canonical_key.as_deref().expect("keyed"))
            .or_default()
            .push(node);
    }
    let mut out: Vec<Vec<&SourceNode>> = by_key
        .into_values()
        .filter(|g| g.iter().any(|n| n.when.is_some()))
        .map(|mut g| {
            g.sort_by_key(|n| n.position);
            g
        })
        .collect();
    out.sort_by_key(|g| g[0].position);
    out
}

/// The alternatives' and placeholders' build-time checks for one mapping.
pub fn check(hub: &CanonicalModel, ir: &MappingIr) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let mut by_scope: BTreeMap<&Scope, Vec<&SourceNode>> = BTreeMap::new();
    for node in ir.nodes.values() {
        by_scope.entry(&node.scope).or_default().push(node);
        if let Some(when) = &node.when {
            check_when(hub, ir, node, when, &mut diags);
        }
        if let Some(placeholder) = &node.placeholder {
            check_placeholder(node, placeholder, &mut diags);
        }
    }
    let warns = ir.meta.no_target_warns();
    for nodes in by_scope.into_values() {
        for group in groups(nodes) {
            check_group(&group, warns, &mut diags);
        }
    }
    diags
}

fn diag(severity: Severity, code: &str, node: &SourceNode, message: String) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity,
        source_node: Some(node.id.to_string()),
        message,
        span: None,
    }
}

/// `E132` for a misplaced `when`, `E130` for an entry that names nothing it
/// can test.
fn check_when(
    hub: &CanonicalModel,
    ir: &MappingIr,
    node: &SourceNode,
    when: &BTreeMap<String, Condition>,
    diags: &mut Vec<Diagnostic>,
) {
    let constant = node.constant.is_some();
    let misplaced = if when.is_empty() {
        Some("an empty `when` always holds; leave it out for the default")
    } else if node.is_collection() || node.clone_of.is_some() || node.lift.is_some() {
        Some(
            "`when` belongs on a keyed scalar or a constant, not a collection, clone or lifted key",
        )
    } else if node.multiple.is_some() {
        Some("`when` cannot be combined with `multiple`")
    } else if constant && node.canonical_key.is_some() {
        Some("a pinned constant always writes its literal; give the `when` to an unkeyed constant")
    } else if !constant && node.canonical_key.is_none() {
        Some("`when` decides whether a value or constant is written; a helper writes nothing")
    } else {
        None
    };
    if let Some(why) = misplaced {
        diags.push(diag(Severity::Error, "E132", node, why.to_string()));
        return;
    }
    let Some(scope) = canonical_scope_of(node, ir) else {
        return; // E011 is reported by the hub.
    };
    for (key, cond) in when {
        let problem = match hub.get(&scope, key) {
            _ if cond.reference().is_some() => Some("a `$` reference is not supported in `when`"),
            _ if cond.problem().is_some() => cond.problem(),
            None => Some("no scalar hub key of this scope has that name"),
            Some(field) if field.is_collection => Some("it names a collection, not a value"),
            Some(field)
                if cond.values().is_some()
                    && matches!(field.ty, MappingType::Decimal | MappingType::Boolean) =>
            {
                Some("a value test compares text; a decimal or boolean key takes `{ present = … }`")
            }
            Some(_) => None,
        };
        if let Some(problem) = problem {
            diags.push(diag(
                Severity::Error,
                "E130",
                node,
                format!("`when` entry `{key}` on `{}`: {problem}", node.id),
            ));
        }
    }
}

/// `E134` for a `placeholder` that has no element of a keyed scalar to fill,
/// or is not one or more literals of the node's type.
fn check_placeholder(node: &SourceNode, placeholder: &Condition, diags: &mut Vec<Diagnostic>) {
    let misplaced = !keyed_scalar(node) || node.constant.is_some() || node.multiple.is_some();
    let problem = if misplaced {
        Some(
            "`placeholder` fills the element of a keyed scalar; it is not valid on a \
             collection, clone, lifted key, constant, helper or `multiple` node"
                .to_string(),
        )
    } else {
        match placeholder.values() {
            None | Some([]) => {
                Some("`placeholder` is a literal or a non-empty list of them".to_string())
            }
            Some(values) => values.iter().find_map(|value| {
                let reason = if value.starts_with('$') {
                    Some("a `$` reference is not a placeholder".to_string())
                } else {
                    constant_literal_error(node.source_type, value)
                };
                reason.map(|reason| {
                    format!(
                        "`{value}` is not a valid `{}` literal: {reason}",
                        node.source_type
                    )
                })
            }),
        }
    };
    if let Some(problem) = problem {
        diags.push(diag(Severity::Error, "E134", node, problem));
    }
}

/// `E132` and `W140` over one set of alternatives (`W140` only while
/// `NO_TARGET` refuses: `no_target = "warn"` chose dropping).
fn check_group(group: &[&SourceNode], no_target_warns: bool, diags: &mut Vec<Diagnostic>) {
    let key = group[0].canonical_key.as_deref().unwrap_or_default();
    let mut default: Option<&SourceNode> = None;
    for (i, node) in group.iter().enumerate() {
        if node.multiple.is_some() {
            diags.push(diag(
                Severity::Error,
                "E132",
                node,
                format!(
                    "`{}` is an alternative of `{key}`, which cannot be `multiple`",
                    node.id
                ),
            ));
        }
        if group.len() > 1 && (node.required || node.default.is_some()) {
            diags.push(diag(
                Severity::Error,
                "E132",
                node,
                format!(
                    "`{}` is one of several alternatives of `{key}`: `required` or `default` would \
                     apply to one element of the value only",
                    node.id
                ),
            ));
        }
        match &node.when {
            None => match default {
                Some(first) => diags.push(diag(
                    Severity::Error,
                    "E132",
                    node,
                    format!(
                        "`{}` and `{}` both lack `when`; at most one alternative of `{key}` is the default",
                        first.id, node.id
                    ),
                )),
                None => default = Some(node),
            },
            Some(when) => {
                if let Some(earlier) = group[..i].iter().find(|e| e.when.as_ref() == Some(when)) {
                    diags.push(diag(
                        Severity::Error,
                        "E132",
                        node,
                        format!(
                            "`{}` repeats the `when` of `{}`, which is chosen first, so it never writes",
                            node.id, earlier.id
                        ),
                    ));
                }
            }
        }
    }
    if default.is_none() && !no_target_warns {
        diags.push(diag(
            Severity::Warning,
            "W140",
            group[0],
            format!(
                "every alternative of `{key}` has a `when`: a value none of them takes is \
                 refused (NO_TARGET)"
            ),
        ));
    }
}
