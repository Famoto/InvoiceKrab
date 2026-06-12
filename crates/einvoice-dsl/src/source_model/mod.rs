//! Typed source-model metadata and synthesis.
//!
//! Rust has no general runtime reflection, so the compiler validates source paths
//! against *metadata* describing the typed source struct tree. This module
//! defines that metadata, the path resolver, and the *synthesizer* that builds
//! the metadata from the mapping nodes themselves ([`synthesize_source_model`]);
//! codegen then emits the typed source structs from the same metadata.
//!
//! # Structure
//!
//! Split by concern into private submodules, re-exported here so callers keep
//! using the flat `source_model::` paths:
//!
//! - [`meta`] — the metadata types: [`SourceModelMeta`] (an id, a root struct,
//!   and a table of named structs), [`StructMeta`] / [`FieldMeta`] /
//!   [`FieldType`], and the [`ResolvedField`] / [`PathError`] resolution outcome.
//! - [`resolve`] — [`resolve_path`] / [`resolve_path_from`], which walk a dotted
//!   path applying the traversal rules and report the resolved leaf.
//! - [`synth`] — [`synthesize_source_model`], which derives the struct tree *and*
//!   every node's `source_path` from the node ids (which mirror the XML element
//!   tree), their `type`/`required`/`xml`, and `[meta].root`.
//!
//! # Traversal rules
//!
//! - `T` → descend into the field if it exists.
//! - `Option<T>` → contributes `optional` (a `None` is missing at runtime).
//! - `Vec<T>` → contributes `repeated` (a collection or multiple values).
//! - A scalar leaf cannot be descended into.
//! - Enum/choice fields are unsupported except through an adapter, so they are
//!   not represented here as descendable structs.
//!
//! # Testing
//!
//! Unit tests live beside each submodule: metadata JSON round-tripping in
//! [`meta`], the path-resolution rules and error paths in [`resolve`], and
//! synthesis (scalar/valued-container/collection shapes, determinism, and E024
//! conflicts) in [`synth`].

mod meta;
mod resolve;
mod synth;

pub use meta::{FieldMeta, FieldType, PathError, ResolvedField, SourceModelMeta, StructMeta};
pub use resolve::{resolve_path, resolve_path_from};
pub use synth::synthesize_source_model;
