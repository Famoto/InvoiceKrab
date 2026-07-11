//! Compile-time mapping DSL for the e-invoice hub transformation engine.
//!
//! This crate implements the **mapping compiler** for KrabInvoice's TOML mapping
//! DSL: it parses a spoke's TOML mapping file into typed source nodes, resolves
//! inheritance + disabled nodes + defaults into a normalized
//! [`MappingIr`](ir::MappingIr), derives the canonical hub from the union of
//! spoke `canonical_key`s, statically validates the result, exposes the IR to
//! static-analysis reports, and generates native Rust mapper code.
//!
//! The guiding principle is **fail at build time, not at runtime**: unknown TOML
//! keys, bad paths, fallback cycles, and type mismatches are all compile-time
//! errors; the runtime executes only generated Rust, never interpreted TOML.
//!
//! # Pipeline
//!
//! ```text
//! TOML mapping ─► parse ─► resolve(inherit, disabled, defaults)
//!              ─► derive hub ─► validate ─► MappingIr ─► reports / codegen
//! ```

pub mod codegen;
pub mod compile;
pub mod error;
pub mod hub;
pub mod ir;
pub mod loader;
pub mod meta;
pub mod multiple;
pub mod node;
pub mod normalize;
pub mod parse;
pub mod report;
pub mod resolve;
pub mod source_model;
pub mod types;
pub mod validate;

pub use codegen::{SpokeDedupPlan, SpokeModule, generate_hub, plan_spoke_dedup};
pub use compile::{CompileOutput, KNOWN_ADAPTERS, SpokeInput, compile, known_adapters};
pub use error::{ConfigError, Diagnostic, Severity};
pub use hub::{CanonicalField, CanonicalModel, CanonicalScope, canonical_scope_of, derive_hub};
pub use ir::{MappingIr, build_ir};
pub use loader::{LoadOutput, LoadedSpoke, load_dir, slug_of};
pub use meta::MappingMeta;
pub use multiple::MultiplePolicy;
pub use node::{NodeId, RawNode, Scope, SourceNode};
pub use normalize::NormalizeOp;
pub use parse::{ParsedMapping, parse_mapping};
pub use report::{
    CoverageMatrix, FieldKey, Gap, coverage_matrix, covered_canonical_fields, fallback_graph,
    gap_report, render_coverage_markdown, required_canonical_fields,
};
pub use source_model::{
    FieldMeta, FieldType, PathError, ResolvedField, SourceModelMeta, StructMeta, resolve_path,
    resolve_path_from, synthesize_source_model,
};
pub use types::MappingType;
pub use validate::{ValidationInput, validate};
