//! Build-time codegen of the typed hub + native spoke mappers.
//!
//! Every `*.toml` in the workspace `mappings/` directory is a spoke. Nothing is
//! hardcoded here: the directory is scanned, and each spoke's identity — its
//! generated module name, its public `Spoke` enum variant, and its display name —
//! is derived from the file's reserved `[meta]` table, specifically
//! `meta.doc_format`. Adding a new format is therefore *only* a
//! matter of dropping a new TOML into `mappings/`.
//!
//! Every spoke is loaded and compiled through the *single* `einvoice-dsl`
//! pipeline — `einvoice_dsl::load_dir` (scan, parse, `inherits` chains,
//! disabled bases, slugs) then `einvoice_dsl::compile` — the same path
//! `cargo run -p einvoice-dsl -- check` uses. The build fails on any
//! error-severity diagnostic from *any* stage, including `validate` (e.g.
//! unknown adapters, bad source paths), so "fail at build time" is enforced by
//! the whole compiler, not a partial reimplementation of it. The result is
//! emitted, into `OUT_DIR`, as:
//!
//! - `hub.rs` — the typed `MainKey` hub, derived from the union of every spoke's
//!   canonical keys.
//! - `<slug>.rs` — one per spoke: its typed source structs, `from_xml`/`to_xml`,
//!   and the `read`/`write` mappers. Spokes generating identical struct text
//!   share one `shared_<n>.rs` structs module instead; a spoke whose whole
//!   module is byte-identical to an earlier one emits no file (aliased module).
//! - `spokes.rs` — the generated glue: a `mod <slug>` per spoke (include or
//!   alias), the shared structs modules, the public `Spoke` enum, and the
//!   `read`/`write` dispatch over it.
//!
//! `compile` synthesizes each spoke's typed source model from its nodes (the ids
//! mirror the XML element tree); `lib.rs` `include!`s the generated code. There is
//! no runtime interpretation and no hand-written model code: everything
//! downstream of the TOML is generated.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use einvoice_dsl::compile::{CompileOutput, SpokeInput};
use einvoice_dsl::ir::MappingIr;
use einvoice_dsl::{
    Severity, SourceModelMeta, compile, covered_canonical_fields, generate_hub,
    generate_mapper_module, generate_source_module, generate_spoke, known_adapters, load_dir,
    required_canonical_fields,
};

/// One discovered spoke: its meta-derived names plus its compiled artifacts.
struct Spoke {
    /// `snake_case` module name and `<slug>.rs` file stem (e.g. `ubl_invoice`).
    slug: String,
    /// `PascalCase` public `Spoke` enum variant (e.g. `UblInvoice`).
    variant: String,
    /// Display id carried into `Spoke::name` (the source-model id from `[meta]`).
    name: String,
    /// Discriminator substrings from `[meta].detect`, carried into
    /// `Spoke::detect_markers` for source auto-detection.
    detect: Vec<String>,
    /// The root XML element name (from `[meta].root`), carried into
    /// `Spoke::root` as the primary signature for source auto-detection.
    root: String,
    /// The compiled, normalized mapping IR.
    ir: MappingIr,
    /// The synthesized typed source model (input to codegen).
    source: SourceModelMeta,
}

fn main() {
    let mappings_dir = workspace_mappings_dir();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when spokes are added or removed, not only when one is edited.
    println!("cargo:rerun-if-changed={}", mappings_dir.display());

    // Load + compile every spoke through the one shared DSL pipeline — the same
    // `load_dir` + `compile` path `cargo run -p einvoice-dsl -- check` uses.
    let loaded = load_dir(&mappings_dir)
        .unwrap_or_else(|e| panic!("loading {}: {e}", mappings_dir.display()));
    for path in &loaded.files {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let inputs: Vec<SpokeInput> = loaded
        .spokes
        .iter()
        .map(|s| SpokeInput {
            id: s.slug.clone(),
            chain: &s.chain,
        })
        .collect();
    let out = compile(&inputs, &known_adapters());
    assert_clean(&out);

    let spokes = collect_spokes(&out);

    // Emit the shared hub once (already derived + validated by `compile`).
    std::fs::write(out_dir.join("hub.rs"), generate_hub(&out.hub)).expect("write hub.rs");

    // Emit the spoke modules, deduplicated by generated content (codegen is
    // byte-deterministic, so equality of text is equality of behavior):
    //
    // 1. Spokes whose synthesized source models generate identical struct text
    //    share one `shared_<n>.rs` structs module and emit mappers-only files
    //    (this collapses the serde-derive cost of `inherits` families like
    //    UBL / XRechnung / Peppol to one expansion).
    // 2. A spoke whose final file is byte-identical to an earlier spoke's
    //    (e.g. an `inherits` child that overrides nothing) emits no file at
    //    all; `spokes.rs` aliases its module to the earlier one.
    // Generated files start with an identity comment block (spoke/model names)
    // that legitimately differs between behaviorally identical spokes, so all
    // equality checks compare the body after the first blank line.
    fn body(text: &str) -> &str {
        text.split_once("\n\n").map_or(text, |(_, b)| b)
    }

    let source_texts: Vec<String> = spokes
        .iter()
        .map(|s| generate_source_module(&s.source))
        .collect();
    // Group spokes by identical struct text, in slug order (deterministic).
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, text) in source_texts.iter().enumerate() {
        match groups.iter_mut().find(|(t, _)| body(t) == body(text)) {
            Some((_, members)) => members.push(i),
            None => groups.push((text, vec![i])),
        }
    }
    let mut shared_mods: Vec<String> = Vec::new();
    let mut structs_module_of: Vec<Option<String>> = vec![None; spokes.len()];
    for (text, members) in &groups {
        if members.len() < 2 {
            continue;
        }
        let name = format!("shared_{}", shared_mods.len());
        let file = format!("{name}.rs");
        std::fs::write(out_dir.join(&file), text).unwrap_or_else(|e| panic!("write {file}: {e}"));
        for &i in members {
            structs_module_of[i] = Some(name.clone());
        }
        shared_mods.push(name);
    }

    let mut seen: Vec<(String, String)> = Vec::new(); // (file text, canonical slug)
    let mut alias_of: Vec<Option<String>> = vec![None; spokes.len()];
    for (i, spoke) in spokes.iter().enumerate() {
        let code = match &structs_module_of[i] {
            Some(shared) => generate_mapper_module(
                &spoke.ir,
                &spoke.source,
                "super::hub",
                &format!("super::{shared}"),
            ),
            None => generate_spoke(&spoke.ir, &spoke.source, "super::hub"),
        };
        if let Some((_, canonical)) = seen.iter().find(|(text, _)| body(text) == body(&code)) {
            alias_of[i] = Some(canonical.clone());
            continue;
        }
        let file = format!("{}.rs", spoke.slug);
        std::fs::write(out_dir.join(&file), &code).unwrap_or_else(|e| panic!("write {file}: {e}"));
        seen.push((code, spoke.slug.clone()));
    }

    // Emit the dispatch glue (module decls + `Spoke` enum + read/write).
    std::fs::write(
        out_dir.join("spokes.rs"),
        generate_dispatch(&spokes, &shared_mods, &alias_of),
    )
    .expect("write spokes.rs");
}

/// Locates the workspace `mappings/` directory (two levels up from the crate).
fn workspace_mappings_dir() -> PathBuf {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crate is two levels under the workspace root")
        .join("mappings")
}

/// Builds the per-spoke codegen descriptors from a clean [`CompileOutput`]. Every
/// name (`variant`, `name`, `detect`) is derived from the compiled IR's `[meta]`,
/// and the `ir` + `source` are the exact artifacts `compile` validated.
fn collect_spokes(out: &CompileOutput) -> Vec<Spoke> {
    out.irs
        .iter()
        .map(|(slug, ir)| {
            let meta = &ir.meta;
            let name = meta
                .source_model
                .clone()
                .unwrap_or_else(|| format!("{}:{}", meta.doc_format, meta.format_version));
            let source = out
                .sources
                .get(slug)
                .unwrap_or_else(|| panic!("compile output missing source for `{slug}`"))
                .clone();
            Spoke {
                slug: slug.clone(),
                variant: pascal_of(&meta.doc_format),
                name,
                detect: meta.detect.clone(),
                root: source.root.clone(),
                ir: ir.clone(),
                source,
            }
        })
        .collect()
}

/// Generates `spokes.rs`: the shared structs modules, a `mod <slug>` per spoke
/// (each `include!`ing its emitted file, or re-exporting a byte-identical
/// sibling), the public `Spoke` enum, and the `read`/`write` dispatch.
fn generate_dispatch(
    spokes: &[Spoke],
    shared_mods: &[String],
    alias_of: &[Option<String>],
) -> String {
    let mut out = String::new();
    out.push_str("// Generated spoke registry + dispatch. Do not edit by hand.\n");
    out.push_str("// One entry per `mappings/*.toml`; names derive from `[meta].doc_format`.\n\n");

    out.push_str("use einvoice_transformator::result::MappingResult;\n");
    out.push_str("use hub::MainKey;\n\n");

    // Structs modules shared by spokes with identical synthesized source models.
    for name in shared_mods {
        let _ = writeln!(out, "mod {name} {{");
        let _ = writeln!(
            out,
            "    include!(concat!(env!(\"OUT_DIR\"), \"/{name}.rs\"));"
        );
        out.push_str("}\n\n");
    }

    // Per-spoke generated module. A spoke whose generated code is byte-identical
    // to an earlier spoke's re-exports that module instead of duplicating it.
    for (spoke, alias) in spokes.iter().zip(alias_of) {
        let _ = writeln!(out, "pub mod {} {{", spoke.slug);
        match alias {
            Some(canonical) => {
                let _ = writeln!(out, "    pub use super::{canonical}::*;");
            }
            None => {
                let _ = writeln!(
                    out,
                    "    include!(concat!(env!(\"OUT_DIR\"), \"/{}.rs\"));",
                    spoke.slug
                );
            }
        }
        out.push_str("}\n\n");
    }

    // The public Spoke enum, with a variant per discovered spoke.
    out.push_str("/// A source/target format handled by a generated mapper.\n");
    out.push_str("///\n");
    out.push_str("/// Variants are generated from each `mappings/*.toml`'s `[meta].doc_format`.\n");
    out.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n");
    out.push_str("pub enum Spoke {\n");
    for spoke in spokes {
        let _ = writeln!(out, "    /// `{}`", spoke.name);
        let _ = writeln!(out, "    {},", spoke.variant);
    }
    out.push_str("}\n\n");

    out.push_str("impl Spoke {\n");
    out.push_str("    /// Every spoke compiled into this build, in slug order.\n");
    out.push_str("    pub const ALL: &'static [Spoke] = &[\n");
    for spoke in spokes {
        let _ = writeln!(out, "        Spoke::{},", spoke.variant);
    }
    out.push_str("    ];\n\n");
    emit_str_accessor(
        &mut out,
        spokes,
        "name",
        "The spoke's display id (its source-model id from `[meta]`).",
        |spoke| &spoke.name,
    );

    // Auto-detection markers from `[meta].detect`.
    out.push_str(
        "    /// Case-insensitive discriminator substrings from `[meta].detect`.\n\
         \x20\x20\x20\x20///\n\
         \x20\x20\x20\x20/// A document matching one of a spoke's markers is recognized as that\n\
         \x20\x20\x20\x20/// format in preference to a base format that declares none.\n",
    );
    out.push_str("    pub fn detect_markers(self) -> &'static [&'static str] {\n");
    out.push_str("        match self {\n");
    for spoke in spokes {
        let markers = spoke
            .detect
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "            Spoke::{} => &[{markers}],", spoke.variant);
    }
    out.push_str("        }\n");
    out.push_str("    }\n\n");

    // The document's root XML element, from `[meta].root` — the primary
    // signature used to identify a source format without trial-parsing.
    emit_str_accessor(
        &mut out,
        spokes,
        "root",
        "The document's root XML element name, from `[meta].root`.\n\
         \n\
         The primary discriminator for source auto-detection: a document is\n\
         narrowed to the spokes sharing its root before markers disambiguate.",
        |spoke| &spoke.root,
    );

    // The spoke's footprint in the shared hub vocabulary: every canonical field
    // it maps, and the subset it marks required. The Mapping Comparison Tool
    // classifies a transform by comparing these sets across two spokes.
    emit_key_accessor(
        &mut out,
        spokes,
        "covered_keys",
        "Every canonical hub field this spoke maps, as scope-qualified labels.",
        |spoke| covered_canonical_fields(&spoke.ir),
    );
    emit_key_accessor(
        &mut out,
        spokes,
        "required_keys",
        "The canonical hub fields this spoke marks `required`, as labels.",
        |spoke| required_canonical_fields(&spoke.ir),
    );

    out.push_str("}\n\n");

    // read dispatch: source bytes -> MainKey.
    out.push_str("/// Deserializes `bytes` for `spoke` and runs its generated reader.\n");
    out.push_str(
        "pub fn read(spoke: Spoke, bytes: &[u8]) -> Result<MappingResult<MainKey>, quick_xml::DeError> {\n",
    );
    out.push_str("    match spoke {\n");
    for spoke in spokes {
        let _ = writeln!(
            out,
            "        Spoke::{v} => {{\n            let source = {s}::from_xml(bytes)?;\n            Ok({s}::read(source))\n        }}",
            v = spoke.variant,
            s = spoke.slug
        );
    }
    out.push_str("    }\n");
    out.push_str("}\n\n");

    // write dispatch: MainKey -> source XML. The hub is consumed: the writers
    // move its values into the target struct instead of cloning them.
    out.push_str("/// Runs `spoke`'s generated writer over `hub` and serializes to XML.\n");
    out.push_str(
        "pub fn write(spoke: Spoke, hub: MainKey) -> Result<MappingResult<String>, quick_xml::SeError> {\n",
    );
    out.push_str("    match spoke {\n");
    for spoke in spokes {
        let _ = writeln!(
            out,
            "        Spoke::{v} => {{\n            let written = {s}::write(hub);\n            let xml = match written.value {{\n                Some(source) => Some({s}::to_xml(&source)?),\n                None => None,\n            }};\n            Ok(MappingResult::new(xml, written.diagnostics))\n        }}",
            v = spoke.variant,
            s = spoke.slug
        );
    }
    out.push_str("    }\n");
    out.push_str("}\n");

    out
}

/// Emits a `pub fn <method>(self) -> &'static [&'static str]` accessor on `Spoke`
/// whose arms are `keys(spoke)` rendered as a sorted string-slice literal. Used
/// for `covered_keys` / `required_keys` so the two share one shape.
fn emit_key_accessor(
    out: &mut String,
    spokes: &[Spoke],
    method: &str,
    doc: &str,
    keys: impl Fn(&Spoke) -> std::collections::BTreeSet<String>,
) {
    let _ = writeln!(out, "    /// {doc}");
    let _ = writeln!(
        out,
        "    pub fn {method}(self) -> &'static [&'static str] {{"
    );
    out.push_str("        match self {\n");
    for spoke in spokes {
        let items = keys(spoke)
            .iter()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "            Spoke::{} => &[{items}],", spoke.variant);
    }
    out.push_str("        }\n");
    out.push_str("    }\n");
}

/// Emits a `pub fn <method>(self) -> &'static str` accessor on `Spoke` whose arms
/// map each spoke to the string literal `value(spoke)`. `doc` may be multi-line;
/// each line becomes a `///` line. Shared by the `name` / `root` accessors so the
/// two scalar accessors don't each hand-roll the same match.
fn emit_str_accessor(
    out: &mut String,
    spokes: &[Spoke],
    method: &str,
    doc: &str,
    value: impl Fn(&Spoke) -> &str,
) {
    for line in doc.lines() {
        let _ = writeln!(out, "    /// {line}");
    }
    let _ = writeln!(out, "    pub fn {method}(self) -> &'static str {{");
    out.push_str("        match self {\n");
    for spoke in spokes {
        let _ = writeln!(
            out,
            "            Spoke::{} => {:?},",
            spoke.variant,
            value(spoke)
        );
    }
    out.push_str("        }\n");
    out.push_str("    }\n\n");
}

/// `PascalCase` enum-variant id from a meta `doc_format` (e.g. `ubl-invoice` →
/// `UblInvoice`).
fn pascal_of(doc_format: &str) -> String {
    let mut out = String::new();
    let mut at_word_start = true;
    for c in doc_format.chars() {
        if c.is_ascii_alphanumeric() {
            if at_word_start {
                out.push(c.to_ascii_uppercase());
            } else {
                out.push(c);
            }
            at_word_start = false;
        } else {
            at_word_start = true;
        }
    }
    out
}

/// Panics with every error-severity diagnostic — from any stage, `validate`
/// included — if the compile was not clean. This is the build-time enforcement
/// of "fail at build time": the build cannot emit code the `xtask` dev CLI
/// (`cargo run -p einvoice-dsl -- check`) would reject.
fn assert_clean(out: &CompileOutput) {
    let errors: Vec<String> = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| {
            let node = d.source_node.as_deref().unwrap_or("-");
            format!(
                "[{}] {} ({node}): {}",
                d.severity.as_str(),
                d.code,
                d.message
            )
        })
        .collect();
    assert!(
        errors.is_empty(),
        "mappings did not compile clean:\n{}",
        errors.join("\n")
    );
}
