//! IR classification for codegen.
//!
//! [`MappingPlan`] is built once per generator pass: a single walk of the IR
//! buckets every mapped node by where it belongs (root scalars/collections and
//! the scalar children / nested collections of each collection node). Lookups
//! are then O(log n) map gets instead of re-scanning every node. All buckets
//! preserve the IR's deterministic id order, since `ir.nodes` is a `BTreeMap`.

use std::collections::BTreeMap;

use crate::ir::MappingIr;
use crate::node::{NodeId, Scope, SourceNode};
use crate::source_model::SourceModelMeta;

/// The invariant context threaded through the reader/writer generators: the IR,
/// the typed source model, and the precomputed [`MappingPlan`]. Bundling these
/// keeps the recursive block generators from carrying them as separate
/// arguments.
pub(super) struct GenCtx<'a> {
    pub(super) ir: &'a MappingIr,
    pub(super) source: &'a SourceModelMeta,
    pub(super) plan: &'a MappingPlan<'a>,
}

/// The per-recursion location for a collection block: how deep it is (for unique
/// loop-variable names), its indent, and the enclosing hub/source variables and
/// source struct it is generated against.
pub(super) struct Frame<'a> {
    pub(super) depth: usize,
    pub(super) indent: usize,
    /// The enclosing hub variable the collection is read from / pushed into.
    pub(super) parent_hub: &'a str,
    /// The enclosing source variable the collection is read from / pushed into.
    pub(super) parent_src: &'a str,
    /// The source struct the collection's `source_path` resolves against.
    pub(super) parent_struct: &'a str,
    /// Whether the enclosing scope's variables are owned (consumable): values
    /// may then be moved out instead of cloned. `false` inside a collection
    /// that had to be iterated by reference (its source path is read twice).
    pub(super) owned: bool,
}

/// The mapped nodes of a spoke, classified by position. Helper nodes (no
/// `canonical_key`) and unmapped collections are excluded.
pub(super) struct MappingPlan<'a> {
    /// Mapped scalar nodes at root scope, in id order.
    pub(super) root_scalars: Vec<&'a SourceNode>,
    /// Mapped collection nodes at root scope, in id order.
    pub(super) root_collections: Vec<&'a SourceNode>,
    /// Mapped scalar children of each collection node, keyed by collection id.
    children_by_collection: BTreeMap<&'a NodeId, Vec<&'a SourceNode>>,
    /// Mapped collections nested directly inside each collection node.
    nested_by_collection: BTreeMap<&'a NodeId, Vec<&'a SourceNode>>,
}

impl<'a> MappingPlan<'a> {
    /// Classifies every node in `ir` in a single pass.
    pub(super) fn build(ir: &'a MappingIr) -> Self {
        let mut root_scalars = Vec::new();
        let mut root_collections = Vec::new();
        let mut children_by_collection: BTreeMap<&NodeId, Vec<&SourceNode>> = BTreeMap::new();
        let mut nested_by_collection: BTreeMap<&NodeId, Vec<&SourceNode>> = BTreeMap::new();

        for node in ir.nodes.values() {
            match &node.scope {
                Scope::Root => {
                    if node.is_collection() {
                        if node.canonical_key.is_some() {
                            root_collections.push(node);
                        }
                    } else if !node.is_helper() {
                        root_scalars.push(node);
                    }
                }
                Scope::Collection(parent) => {
                    if node.is_collection() {
                        if node.canonical_key.is_some() {
                            nested_by_collection.entry(parent).or_default().push(node);
                        }
                    } else if !node.is_helper() {
                        children_by_collection.entry(parent).or_default().push(node);
                    }
                }
            }
        }

        Self {
            root_scalars,
            root_collections,
            children_by_collection,
            nested_by_collection,
        }
    }

    /// The mapped scalar children of a collection node, in id order.
    pub(super) fn children_of(&self, coll: &NodeId) -> &[&'a SourceNode] {
        self.children_by_collection
            .get(coll)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The mapped collections nested directly inside a collection node, in id
    /// order.
    pub(super) fn nested_collections_of(&self, coll: &NodeId) -> &[&'a SourceNode] {
        self.nested_by_collection
            .get(coll)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}
