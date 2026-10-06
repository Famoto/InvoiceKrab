//! Build-time codegen of the typed hub + native spoke mappers.
//!
//! Every `*.toml` in the workspace `config/mappings/` directory is a spoke, and
//! every `*.toml` in `config/codecs/` a set of shared lexical codecs. Nothing is
//! hardcoded here: the directories are scanned, and each spoke's identity — its
//! generated module name, its public `Spoke` enum variant, and its display name —
//! is derived from the file's reserved `[meta]` table, specifically
//! `meta.doc_format`. Adding a new format is therefore *only* a
//! matter of dropping a new TOML into `config/mappings/`.
//!
//! Every spoke is loaded and compiled through the *single* `einvoice-dsl`
//! pipeline — `einvoice_dsl::load_config` (codecs, then scan, parse, `inherits`
//! chains, disabled bases, slugs) then `einvoice_dsl::compile` — the same path
//! `cargo run -p einvoice-dsl -- check` uses. The build fails on any
//! error-severity diagnostic from *any* stage, including `validate` (e.g.
//! unknown codecs, bad source paths), so "fail at build time" is enforced by
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
//!   alias), the shared structs modules, the public `Spoke` enum with each
//!   spoke's embedded transformation contract (`Spoke::contract`) and its
//!   schema-conformance declarations (`Spoke::schema`, `Spoke::samples`), and
//!   the `read`/`write` dispatch over it.
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
    LoadedSpoke, SchemaMeta, Severity, SourceModelMeta, SpokeContract, SpokeDedupPlan, SpokeModule,
    check_derivations, compile, generate_hub_with, load_config, plan_spoke_dedup, render_contract,
    spoke_contract,
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
    /// The spoke's transformation contract (embedded in the registry).
    contract: SpokeContract,
    /// The effective `[meta.schema]` (inherited by a CIUS), carried into
    /// `Spoke::schema`.
    schema: Option<SchemaMeta>,
    /// The workspace-relative sample documents this spoke reads, carried into
    /// `Spoke::samples`.
    samples: Vec<String>,
}

fn main() {
    let config_dir = workspace_config_dir();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when codecs or spokes are added or removed, not only when one is
    // edited.
    for dir in ["mappings", "codecs", "derivations.toml"] {
        println!("cargo:rerun-if-changed={}", config_dir.join(dir).display());
    }

    // Load + compile every codec and spoke through the one shared DSL pipeline
    // — the same `load_config` + `compile` path `cargo run -p einvoice-dsl --
    // check` uses.
    let loaded = load_config(&config_dir)
        .unwrap_or_else(|e| panic!("loading {}: {e}", config_dir.display()));
    // The declared schema and sample files too: the loader checked they
    // exist, and a deleted one must fail the next build (E100).
    for path in loaded.files.iter().chain(&loaded.declared_files) {
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
    let mut out = compile(&inputs, &loaded.codecs);
    // The derivation rules are checked against the hub the spokes derived.
    out.diagnostics
        .extend(check_derivations(&out.hub, &loaded.derivations));
    assert_clean(&out);

    let spokes = collect_spokes(&out, &loaded.spokes);

    // Emit the shared hub once (already derived + validated by `compile`).
    std::fs::write(
        out_dir.join("hub.rs"),
        generate_hub_with(&out.hub, &loaded.derivations),
    )
    .expect("write hub.rs");

    // Plan the deduplicated spoke modules (shared structs modules for spokes
    // with identical synthesized source models, aliases for byte-identical
    // spokes — see `plan_spoke_dedup`), then write what the plan emitted.
    let triples: Vec<(&str, &MappingIr, &SourceModelMeta)> = spokes
        .iter()
        .map(|s| (s.slug.as_str(), &s.ir, &s.source))
        .collect();
    let plan = plan_spoke_dedup(&triples, &loaded.codecs, "super::hub");

    for (name, text) in &plan.shared_modules {
        let file = format!("{name}.rs");
        std::fs::write(out_dir.join(&file), text).unwrap_or_else(|e| panic!("write {file}: {e}"));
    }
    for (spoke, module) in spokes.iter().zip(&plan.modules) {
        let SpokeModule::Emit(code) = module else {
            continue; // aliased: no file of its own
        };
        let file = format!("{}.rs", spoke.slug);
        std::fs::write(out_dir.join(&file), code).unwrap_or_else(|e| panic!("write {file}: {e}"));
    }

    // Emit the dispatch glue (module decls + `Spoke` enum + read/write).
    std::fs::write(out_dir.join("spokes.rs"), generate_dispatch(&spokes, &plan))
        .expect("write spokes.rs");
}

/// Locates the workspace `config/` directory (two levels up from the crate).
fn workspace_config_dir() -> PathBuf {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crate is two levels under the workspace root")
        .join("config")
}

/// Builds the per-spoke codegen descriptors from a clean [`CompileOutput`]. Every
/// name (`variant`, `name`, `detect`) is derived from the compiled IR's `[meta]`,
/// and the `ir` + `source` are the exact artifacts `compile` validated; the
/// samples each spoke reads come from the loader (`loaded`).
fn collect_spokes(out: &CompileOutput, loaded: &[LoadedSpoke]) -> Vec<Spoke> {
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
            let contract = spoke_contract(&name, ir, &source);
            let samples = loaded
                .iter()
                .find(|l| &l.slug == slug)
                .unwrap_or_else(|| panic!("loader output missing spoke `{slug}`"))
                .samples
                .clone();
            Spoke {
                slug: slug.clone(),
                variant: pascal_of(&meta.doc_format),
                name,
                detect: meta.detect.clone(),
                root: source.root.clone(),
                ir: ir.clone(),
                source,
                contract,
                schema: meta.schema.clone(),
                samples,
            }
        })
        .collect()
}

/// Generates `spokes.rs`: the shared structs modules, a `mod <slug>` per spoke
/// (each `include!`ing its emitted file, or re-exporting a byte-identical
/// sibling per the dedup `plan`), the public `Spoke` enum, and the
/// `read`/`write` dispatch.
fn generate_dispatch(spokes: &[Spoke], plan: &SpokeDedupPlan) -> String {
    let mut out = String::new();
    out.push_str("// Generated spoke registry + dispatch. Do not edit by hand.\n");
    out.push_str(
        "// One entry per `config/mappings/*.toml`; names derive from `[meta].doc_format`.\n\n",
    );

    out.push_str("use einvoice_transformator::result::MappingResult;\n");
    out.push_str("use hub::MainKey;\n\n");

    // Structs modules shared by spokes with identical synthesized source models.
    for (name, _) in &plan.shared_modules {
        let _ = writeln!(out, "mod {name} {{");
        let _ = writeln!(
            out,
            "    include!(concat!(env!(\"OUT_DIR\"), \"/{name}.rs\"));"
        );
        out.push_str("}\n\n");
    }

    // Per-spoke generated module. A spoke whose generated code is byte-identical
    // to an earlier spoke's re-exports that module instead of duplicating it.
    for (spoke, module) in spokes.iter().zip(&plan.modules) {
        let _ = writeln!(out, "pub mod {} {{", spoke.slug);
        match module {
            SpokeModule::Alias(canonical) => {
                let _ = writeln!(out, "    pub use super::{canonical}::*;");
            }
            SpokeModule::Emit(_) => {
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
    out.push_str(
        "/// Variants are generated from each `config/mappings/*.toml`'s `[meta].doc_format`.\n",
    );
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
        let _ = writeln!(
            out,
            "            Spoke::{} => &[{}],",
            spoke.variant,
            str_list(&spoke.detect)
        );
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

    // The spoke's transformation contract: what it maps, what it requires on
    // write, what it declares it may lose. Transform analysis compares two.
    out.push_str(
        "    /// The spoke's transformation contract: the canonical keys it maps, the\n\
         \x20\x20\x20\x20/// write routes of its `required` nodes, and the loss it declares.\n",
    );
    out.push_str(
        "    pub fn contract(self) -> &'static crate::contract::TransformationContract {\n",
    );
    out.push_str("        match self {\n");
    for spoke in spokes {
        let _ = writeln!(
            out,
            "            Spoke::{} => &{},",
            spoke.variant,
            contract_static(spoke)
        );
    }
    out.push_str("        }\n");
    out.push_str("    }\n\n");

    // The schema-conformance declarations: the XSD the spoke's documents are
    // validated against, and the sample documents it reads.
    out.push_str(
        "    /// The XSD this spoke's documents are defined by, from `[meta.schema]`\n\
         \x20\x20\x20\x20/// (inherited by a CIUS); `None` when the mapping declares none.\n",
    );
    out.push_str("    pub fn schema(self) -> Option<&'static crate::conformance::Schema> {\n");
    out.push_str("        match self {\n");
    for spoke in spokes {
        let schema = match &spoke.schema {
            None => "None".to_string(),
            Some(schema) => format!(
                "Some(&crate::conformance::Schema {{ xsd: {:?}, catalog: {}, known_gaps: &[{}], refuses: &[{}], schematron: &[{}] }})",
                schema.xsd,
                match &schema.catalog {
                    Some(catalog) => format!("Some({catalog:?})"),
                    None => "None".to_string(),
                },
                str_list(&schema.known_gaps),
                str_list(&schema.refuses),
                str_list(&schema.schematron)
            ),
        };
        let _ = writeln!(out, "            Spoke::{} => {schema},", spoke.variant);
    }
    out.push_str("        }\n");
    out.push_str("    }\n\n");
    out.push_str(
        "    /// The sample documents this spoke reads, from `[[meta.samples]]`:\n\
         \x20\x20\x20\x20/// workspace-relative paths, each to be schema-valid itself and to\n\
         \x20\x20\x20\x20/// round-trip through every spoke with a schema.\n",
    );
    out.push_str("    pub fn samples(self) -> &'static [&'static str] {\n");
    out.push_str("        match self {\n");
    for spoke in spokes {
        let _ = writeln!(
            out,
            "            Spoke::{} => &[{}],",
            spoke.variant,
            str_list(&spoke.samples)
        );
    }
    out.push_str("        }\n");
    out.push_str("    }\n");

    out.push_str("}\n\n");

    for spoke in spokes {
        let _ = writeln!(
            out,
            "/// The embedded transformation contract of `{}`.\nstatic {}: crate::contract::TransformationContract = {};\n",
            spoke.name,
            contract_static(spoke),
            render_contract(&spoke.contract, "crate::contract")
        );
    }

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

/// `items` as the elements of a Rust string-slice literal (`"a", "b"`).
fn str_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("{item:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The name of the `static` holding a spoke's embedded contract
/// (`UBL_INVOICE_CONTRACT`).
fn contract_static(spoke: &Spoke) -> String {
    format!("{}_CONTRACT", spoke.slug.to_uppercase())
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
