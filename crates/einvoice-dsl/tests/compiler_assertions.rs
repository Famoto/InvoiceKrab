//! Black-box tests of the compiler's documented assertions.
//!
//! Every case writes a small `config/` directory (the repository's shared
//! codecs plus the case's mappings), loads it through [`load_config`] — the
//! loader the build script and `xtask check` use — compiles it, and checks
//! the diagnostic codes against the reference table in
//! `config/mappings/README.md`.
//!
//! A case the compiler accepts is also run through codegen, and the generated
//! hub and spoke modules are parsed with `syn` and checked for duplicate
//! struct, field and XML bindings, so "what `check` accepts, the build
//! accepts" is asserted here rather than discovered by rustc.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use einvoice_dsl::{
    CodecTable, CompileOutput, MappingIr, Severity, SourceModelMeta, SpokeInput, SpokeModule,
    compile, generate_hub, load_config, plan_spoke_dedup,
};
use rstest::rstest;

// --- harness ---------------------------------------------------------------

/// A minimal `[meta]` table for a spoke with `doc_format = fmt`, root `Doc`
/// in a namespace of its own (`urn:<fmt>`), so spokes of one case never share
/// a root and need no `[meta.identity]` to tell them apart (E121). An `extra`
/// declaring its own `[meta.namespaces]` replaces that default.
fn meta(fmt: &str, extra: &str) -> String {
    let namespaces = if extra.contains("[meta.namespaces]") {
        String::new()
    } else {
        format!("[meta.namespaces]\n\"\" = \"urn:{fmt}\"\n")
    };
    format!(
        "[meta]\ndoc_format = \"{fmt}\"\nformat_version = \"1\"\nmapping_version = \"1\"\n\
         canonical_model = \"c:1\"\nroot = \"Doc\"\n{extra}\n{namespaces}"
    )
}

/// One keyed root node, so a case has something valid besides its subject.
const KEY: &str = "[Doc.ID]\ntype = \"identifier\"\ncanonical_key = \"InvoiceNumber\"\n";

/// A single spoke `t` with `body` after its `[meta]`.
fn one(body: &str) -> Vec<(String, String)> {
    vec![("t.toml".into(), format!("{}{body}", meta("t", "")))]
}

/// A fresh `config/` directory holding the repository's codecs and `files`
/// (mapping file name → TOML). Returns the config dir.
fn config_dir(files: &[(String, String)]) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "einvoice-assertions-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let config = root.join("config");
    std::fs::create_dir_all(config.join("mappings")).unwrap();
    std::fs::create_dir_all(config.join("codecs")).unwrap();
    let codecs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/codecs");
    for entry in std::fs::read_dir(codecs).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, config.join("codecs").join(path.file_name().unwrap())).unwrap();
    }
    for (name, body) in files {
        std::fs::write(config.join("mappings").join(name), body).unwrap();
    }
    config
}

/// The result of loading and compiling one case.
enum Outcome {
    /// The loader refused the config (unknown fields, inheritance, E100/E101).
    LoadError(String),
    /// The config compiled; `codes` are every diagnostic code, sorted.
    Compiled {
        codes: Vec<String>,
        out: CompileOutput,
        slugs: Vec<String>,
        codecs: CodecTable,
    },
}

fn run(files: &[(String, String)]) -> Outcome {
    let dir = config_dir(files);
    let loaded = match load_config(&dir) {
        Ok(l) => l,
        Err(e) => return Outcome::LoadError(e.message),
    };
    let spokes: Vec<SpokeInput> = loaded
        .spokes
        .iter()
        .map(|s| SpokeInput {
            id: s.slug.clone(),
            chain: &s.chain,
        })
        .collect();
    let out = compile(&spokes, &loaded.codecs);
    let mut codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.clone()).collect();
    codes.sort();
    codes.dedup();
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    Outcome::Compiled {
        codes,
        out,
        slugs: loaded.spokes.iter().map(|s| s.slug.clone()).collect(),
        codecs: loaded.codecs,
    }
}

/// Asserts the case compiles with error `code` among its diagnostics.
fn assert_rejected(files: &[(String, String)], code: &str) {
    match run(files) {
        Outcome::LoadError(e) => panic!("expected {code}, but loading failed: {e}"),
        Outcome::Compiled { codes, out, .. } => {
            assert!(
                codes.iter().any(|c| c == code),
                "expected {code}, got {codes:?}"
            );
            assert!(out.has_errors(), "{code} must fail the build");
        }
    }
}

/// Asserts the case is clean (warnings allowed) and its generated code is
/// well-formed Rust whose structs bind every field and XML name once.
fn assert_accepted(files: &[(String, String)]) {
    let (out, slugs, codecs) = match run(files) {
        Outcome::LoadError(e) => panic!("expected a clean compile, but loading failed: {e}"),
        Outcome::Compiled {
            out, slugs, codecs, ..
        } => (out, slugs, codecs),
    };
    let errors: Vec<_> = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "expected a clean compile, got {errors:#?}"
    );
    assert_codegen_well_formed(&out, &slugs, &codecs);
}

/// Asserts the loader refuses the config, with `needle` in the message.
fn assert_load_error(files: &[(String, String)], needle: &str) {
    match run(files) {
        Outcome::LoadError(e) => assert!(e.contains(needle), "`{needle}` not in: {e}"),
        Outcome::Compiled { codes, .. } => panic!("expected a load error, compiled: {codes:?}"),
    }
}

/// Generates the hub and every spoke module and checks each parses as Rust,
/// that no file defines a struct twice, that no struct repeats a field or an
/// XML binding, and that no spoke struct shadows a hub type it glob-imports.
fn assert_codegen_well_formed(out: &CompileOutput, slugs: &[String], codecs: &CodecTable) {
    let hub = generate_hub(&out.hub);
    let hub_types = check_file("hub.rs", &hub);
    let triples: Vec<(&str, &MappingIr, &SourceModelMeta)> = slugs
        .iter()
        .map(|s| (s.as_str(), &out.irs[s], &out.sources[s]))
        .collect();
    let plan = plan_spoke_dedup(&triples, codecs, "super::hub");
    let mut texts: Vec<(String, String)> = plan.shared_modules.clone();
    for (slug, module) in slugs.iter().zip(&plan.modules) {
        if let SpokeModule::Emit(text) = module {
            texts.push((slug.clone(), text.clone()));
        }
    }
    for (name, text) in &texts {
        let structs = check_file(name, text);
        let shadowed: Vec<_> = structs.intersection(&hub_types).collect();
        assert!(
            shadowed.is_empty(),
            "{name}: structs {shadowed:?} shadow glob-imported hub types"
        );
    }
}

/// Parses one generated file and checks its structs; returns their names.
fn check_file(name: &str, text: &str) -> BTreeSet<String> {
    let file = syn::parse_file(text)
        .unwrap_or_else(|e| panic!("{name} is not valid Rust: {e}\n----\n{text}"));
    let mut names = BTreeSet::new();
    for item in &file.items {
        let syn::Item::Struct(s) = item else { continue };
        let ident = s.ident.to_string();
        assert!(
            names.insert(ident.clone()),
            "{name}: struct `{ident}` defined twice"
        );
        let mut fields = BTreeSet::new();
        let mut renames = BTreeMap::new();
        for field in &s.fields {
            if let Some(id) = &field.ident {
                assert!(
                    fields.insert(id.to_string()),
                    "{name}: `{ident}.{id}` twice"
                );
            }
            if let Some(xml) = serde_rename(field) {
                let fid = field.ident.as_ref().map(ToString::to_string);
                if let Some(prev) = renames.insert(xml.clone(), fid.clone()) {
                    panic!("{name}: `{ident}` binds XML `{xml}` twice ({prev:?}, {fid:?})");
                }
            }
        }
    }
    names
}

/// The `#[serde(rename = "…")]` of a field, if any.
fn serde_rename(field: &syn::Field) -> Option<String> {
    let mut found = None;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let _ = attr.parse_nested_meta(|m| {
            if m.path.is_ident("rename") {
                let lit: syn::LitStr = m.value()?.parse()?;
                found = Some(lit.value());
            } else if m.input.peek(syn::Token![=]) {
                let _: syn::Expr = m.value()?.parse()?;
            }
            Ok(())
        });
    }
    found
}

// --- the shipped mappings ----------------------------------------------------

#[test]
fn test_shipped_config_is_clean_and_generates_well_formed_code() {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let loaded = load_config(&config).expect("shipped config loads");
    let spokes: Vec<SpokeInput> = loaded
        .spokes
        .iter()
        .map(|s| SpokeInput {
            id: s.slug.clone(),
            chain: &s.chain,
        })
        .collect();
    let out = compile(&spokes, &loaded.codecs);
    assert!(out.diagnostics.is_empty(), "{:#?}", out.diagnostics);
    let slugs: Vec<String> = loaded.spokes.iter().map(|s| s.slug.clone()).collect();
    assert_codegen_well_formed(&out, &slugs, &loaded.codecs);
}

// --- accepted mappings ---------------------------------------------------------

#[rstest]
#[case::minimal(KEY)]
#[case::helper_fallback_string_to_identifier(
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"identifier\"\n"
)]
#[case::valued_element(
    "[Doc.A]\ntype=\"decimal\"\ncanonical_key=\"A\"\n[Doc.A.c]\nxml=\"@c\"\ntype=\"currency\"\ncanonical_key=\"AC\"\n"
)]
#[case::clone_root_from_collection(
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\n[Doc.L.X]\ntype=\"identifier\"\nclone_of=\"$root.InvoiceNumber\"\n"
)]
#[case::clone_parent_in_nested_collection(
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\n[Doc.L.ID]\ntype=\"string\"\ncanonical_key=\"LID\"\n[Doc.L.S]\ntype=\"collection\"\ncanonical_key=\"Ss\"\n[Doc.L.S.X]\ntype=\"string\"\nclone_of=\"$parent.LID\"\n"
)]
#[case::codec_with_wire("[Doc.D]\ntype=\"date\"\ncanonical_key=\"D\"\ncodec=\"cii-date-102\"\n")]
#[case::boolean_codec("[Doc.A]\ntype=\"boolean\"\ncanonical_key=\"K\"\ncodec=\"boolean-1-0\"\n")]
#[case::selector(concat!(
    "[Doc.Ref]\ntype=\"collection\"\ncanonical_key=\"Refs\"\n[Doc.Ref.ID]\ntype=\"identifier\"\ncanonical_key=\"RefId\"\n",
    "[Doc.Ref.TypeCode]\ntype=\"string\"\nconstant=\"916\"\n",
    "[Doc.Tender]\nxml=\"Ref\"\nmatch={\"TypeCode\"=\"50\"}\n[Doc.Tender.ID]\ntype=\"identifier\"\ncanonical_key=\"TenderRef\"\n"
))]
#[case::join_with_empty_separator(
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nmultiple=\"join\"\njoin_with=\"\"\n"
)]
#[case::datetime_constant_with_zone(
    "[Doc.A]\ntype=\"datetime\"\nconstant=\"2024-01-01T10:00:00+01:00\"\n"
)]
// Element names that are XML-valid but not Rust identifiers as-is: codegen
// escapes them instead of emitting code rustc rejects.
#[case::keyword_element("[Doc.type]\ntype=\"string\"\ncanonical_key=\"SegA\"\n")]
#[case::self_element("[Doc.Self]\ntype=\"string\"\ncanonical_key=\"SegB\"\n")]
#[case::hyphenated_element("[Doc.\"my-elem\"]\ntype=\"string\"\ncanonical_key=\"SegC\"\n")]
#[case::dotted_rename("[Doc.X]\ntype=\"string\"\ncanonical_key=\"SegD\"\nxml=\"my.elem\"\n")]
#[case::keyword_interior("[Doc.loop.Self.ID]\ntype=\"string\"\ncanonical_key=\"SegE\"\n")]
#[case::keyword_attribute(
    "[Doc.A]\ntype=\"decimal\"\ncanonical_key=\"A\"\n[Doc.A.ref]\nxml=\"@ref\"\ntype=\"string\"\ncanonical_key=\"AT\"\n"
)]
#[case::keyword_collection(
    "[Doc.loop]\ntype=\"collection\"\ncanonical_key=\"Loops\"\n[Doc.loop.ID]\ntype=\"string\"\ncanonical_key=\"LoopId\"\n"
)]
// Interior elements named like types the generated code uses are renamed.
#[case::element_named_like_std_type("[Doc.Option.ID]\ntype=\"string\"\ncanonical_key=\"OptId\"\n")]
#[case::element_named_like_imported_type(
    "[Doc.Decimal.ID]\ntype=\"string\"\ncanonical_key=\"DecId\"\n"
)]
#[case::element_named_like_main_key(
    "[Doc.MainKey.ID]\ntype=\"string\"\ncanonical_key=\"Shadow\"\n"
)]
#[case::element_named_like_root("[Doc.Doc.ID]\ntype=\"string\"\ncanonical_key=\"InnerId\"\n")]
// `A.BC` and `AB.C` both camel-case to `ABC`: two structs, not one merged one.
#[case::struct_name_collision(
    "[Doc.A.BC.X]\ntype=\"string\"\ncanonical_key=\"K1\"\n[Doc.AB.C.Y]\ntype=\"string\"\ncanonical_key=\"K2\"\n"
)]
// A canonical key whose field name is a keyword (`Type` → `type`) is escaped.
#[case::keyword_canonical_key("[Doc.X]\ntype=\"string\"\ncanonical_key=\"Type\"\n")]
#[case::keyword_collection_key(
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Match\"\n[Doc.L.X]\ntype=\"string\"\ncanonical_key=\"Loop\"\n"
)]
fn test_accepted(#[case] body: &str) {
    assert_accepted(&one(body));
}

#[test]
fn test_required_key_shared_with_another_spoke_is_clean() {
    assert_accepted(&[
        (
            "a.toml".into(),
            meta("a", "") + "[Doc.X]\ntype=\"string\"\ncanonical_key=\"Sh\"\nrequired=true\n",
        ),
        (
            "b.toml".into(),
            meta("b", "") + "[Doc.Y]\ntype=\"string\"\ncanonical_key=\"Sh\"\n",
        ),
    ]);
}

#[test]
fn test_inheritance_merge_and_disabled_base_are_clean() {
    assert_accepted(&[
        ("base.toml".into(), meta("base", "disabled = true") + KEY),
        (
            "child.toml".into(),
            meta("child", "inherits = \"base:1\"") + "[Doc.ID]\nrequired = true\n",
        ),
    ]);
}

// --- rejected mappings: one case per diagnostic code ---------------------------

#[rstest]
#[case::e002_missing_type("E002", "[Doc.ID]\ncanonical_key=\"X\"\n")]
#[case::e011_key_in_unkeyed_collection(
    "E011",
    "[Doc.Line]\ntype=\"collection\"\n[Doc.Line.ID]\ntype=\"string\"\ncanonical_key=\"LineId\"\n"
)]
#[case::e012_rust_field_collision(
    "E012",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"FooBar\"\n[Doc.B]\ntype=\"string\"\ncanonical_key=\"Foo_bar\"\n"
)]
#[case::e012_collection_key_in_two_scopes(
    "E012",
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\n[Doc.L.Sub]\ntype=\"collection\"\ncanonical_key=\"Subs\"\n[Doc.L.Sub.X]\ntype=\"string\"\ncanonical_key=\"SX\"\n[Doc.M]\ntype=\"collection\"\ncanonical_key=\"Subs\"\n[Doc.M.X]\ntype=\"string\"\ncanonical_key=\"SX\"\n"
)]
// An element struct named like a hub type would shadow the glob import.
#[case::e012_element_struct_shadows_hub_item(
    "E012",
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\n[Doc.L.X]\ntype=\"string\"\ncanonical_key=\"LX\"\n[Doc.Ls.Item.ID]\ntype=\"string\"\ncanonical_key=\"Shadow\"\n"
)]
#[case::e013_two_primaries(
    "E013",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\n[Doc.B]\ntype=\"string\"\ncanonical_key=\"K\"\n"
)]
#[case::e014_empty_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"\"\n")]
#[case::e014_hyphen_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"Foo-Bar\"\n")]
#[case::e014_space_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"Foo Bar\"\n")]
#[case::e014_digit_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"1Abc\"\n")]
#[case::e014_lowercase_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"type\"\n")]
#[case::e014_self_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"Self\"\n")]
#[case::e014_underscore_key("E014", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"Foo_Bar\"\n")]
#[case::e014_clone_of_key(
    "E014",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\nclone_of=\"$root.bad-key\"\n"
)]
#[case::e024_multiple_on_attribute(
    "E024",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\n[Doc.A.at]\nxml=\"@at\"\ntype=\"string\"\ncanonical_key=\"K2\"\nmultiple=\"first\"\n"
)]
#[case::e024_multiple_on_collection(
    "E024",
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\nmultiple=\"first\"\n[Doc.L.A]\ntype=\"string\"\ncanonical_key=\"K\"\n"
)]
// Two nodes binding one element: `[ID]` and `[Doc.ID]` are the same element
// (the root segment is optional), and so is a node renamed onto it.
#[case::e025_root_prefixed_and_bare(
    "E025",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[ID]\ntype=\"identifier\"\ncanonical_key=\"Other\"\n"
)]
#[case::e025_renamed_onto_element(
    "E025",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\ncanonical_key=\"Other\"\nxml=\"ID\"\n"
)]
#[case::e025_renamed_with_other_type(
    "E025",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"decimal\"\ncanonical_key=\"Amt\"\nxml=\"ID\"\n"
)]
#[case::e025_two_nodes_one_attribute(
    "E025",
    "[Doc.A]\ntype=\"decimal\"\ncanonical_key=\"A\"\n[Doc.A.c]\nxml=\"@c\"\ntype=\"currency\"\ncanonical_key=\"C1\"\n[Doc.A.d]\nxml=\"@c\"\ntype=\"currency\"\ncanonical_key=\"C2\"\n"
)]
#[case::e025_helper_on_primary_element(
    "E025",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\nxml=\"ID\"\n"
)]
#[case::e026_space_in_element("E026", "[Doc.\"a b\"]\ntype=\"string\"\ncanonical_key=\"K\"\n")]
#[case::e026_digit_element("E026", "[Doc.\"1st\"]\ntype=\"string\"\ncanonical_key=\"K\"\n")]
#[case::e026_bad_rename(
    "E026",
    "[Doc.X]\ntype=\"string\"\ncanonical_key=\"K\"\nxml=\"bad name<>\"\n"
)]
#[case::e026_prefixed_rename(
    "E026",
    "[Doc.X]\ntype=\"string\"\ncanonical_key=\"K\"\nxml=\"cbc:X\"\n"
)]
#[case::e026_empty_attribute(
    "E026",
    "[Doc.A]\ntype=\"decimal\"\ncanonical_key=\"A\"\n[Doc.A.c]\nxml=\"@\"\ntype=\"string\"\ncanonical_key=\"AC\"\n"
)]
#[case::e030_fallback_missing(
    "E030",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.Nope\"]\n"
)]
#[case::e030_fallback_disabled(
    "E030",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"string\"\ndisabled=true\n"
)]
#[case::e031_fallback_type(
    "E031",
    "[Doc.A]\ntype=\"date\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"decimal\"\n"
)]
#[case::e032_fallback_scope(
    "E032",
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\n[Doc.L.A]\ntype=\"string\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"string\"\n"
)]
#[case::e033_cycle(
    "E033",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"string\"\nfallbacks=[\"Doc.A\"]\n"
)]
#[case::e033_self(
    "E033",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nfallbacks=[\"Doc.A\"]\n"
)]
#[case::e040_join_without_separator(
    "E040",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nmultiple=\"join\"\n"
)]
#[case::e040_separator_without_join(
    "E040",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\njoin_with=\",\"\n"
)]
#[case::e043_multiple_and_fallbacks(
    "E043",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\nmultiple=\"first\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"string\"\n"
)]
#[case::e060_constant_on_collection(
    "E060",
    "[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\nconstant=\"x\"\n[Doc.L.A]\ntype=\"string\"\ncanonical_key=\"K\"\n"
)]
#[case::e061_month_13("E061", "[Doc.A]\ntype=\"date\"\nconstant=\"2026-13-01\"\n")]
#[case::e061_datetime_garbage(
    "E061",
    "[Doc.A]\ntype=\"datetime\"\nconstant=\"2026-01-01Tgarbage\"\n"
)]
#[case::e061_unit_code("E061", "[Doc.A]\ntype=\"unit_code\"\nconstant=\"not a unit!!\"\n")]
#[case::e061_decimal_comma("E061", "[Doc.A]\ntype=\"decimal\"\nconstant=\"1,5\"\n")]
#[case::e061_currency_lowercase("E061", "[Doc.A]\ntype=\"currency\"\nconstant=\"eur\"\n")]
#[case::e061_boolean("E061", "[Doc.A]\ntype=\"boolean\"\nconstant=\"yes\"\n")]
#[case::e061_blank_identifier("E061", "[Doc.A]\ntype=\"identifier\"\nconstant=\"  \"\n")]
#[case::e062_constant_fallbacks(
    "E062",
    "[Doc.A]\ntype=\"string\"\nconstant=\"x\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"string\"\n"
)]
#[case::e062_constant_multiple(
    "E062",
    "[Doc.A]\ntype=\"string\"\nconstant=\"x\"\nmultiple=\"first\"\n"
)]
#[case::e062_constant_codec(
    "E062",
    "[Doc.A]\ntype=\"date\"\nconstant=\"2026-01-01\"\ncodec=\"date-iso\"\n"
)]
#[case::e070_clone_with_key(
    "E070",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\ncanonical_key=\"Other\"\nclone_of=\"InvoiceNumber\"\n"
)]
#[case::e070_clone_collection(
    "E070",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.L]\ntype=\"collection\"\nclone_of=\"InvoiceNumber\"\n[Doc.L.X]\ntype=\"string\"\n"
)]
#[case::e071_clone_target_missing(
    "E071",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\nclone_of=\"Nope\"\n"
)]
#[case::e071_parent_key_absent(
    "E071",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.L]\ntype=\"collection\"\ncanonical_key=\"Ls\"\n[Doc.L.S]\ntype=\"collection\"\ncanonical_key=\"Ss\"\n[Doc.L.S.X]\ntype=\"identifier\"\nclone_of=\"$parent.InvoiceNumber\"\n"
)]
#[case::e072_clone_type(
    "E072",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"string\"\nclone_of=\"InvoiceNumber\"\n"
)]
#[case::e080_node_ns("E080", "[Doc.X]\ntype=\"string\"\ncanonical_key=\"K\"\nns=\"zz\"\n")]
#[case::e083_dangling_structural(
    "E083",
    "[Doc.ID]\ntype=\"string\"\ncanonical_key=\"K\"\n[Doc.Empty]\nns=\"\"\n"
)]
#[case::e083_structural_root(
    "E083",
    "[Doc.ID]\ntype=\"string\"\ncanonical_key=\"K\"\n[Doc]\nrequired=true\n"
)]
#[case::e084_unknown_codec(
    "E084",
    "[Doc.D]\ntype=\"date\"\ncanonical_key=\"D\"\ncodec=\"nope\"\n"
)]
#[case::e085_codec_type(
    "E085",
    "[Doc.D]\ntype=\"datetime\"\ncanonical_key=\"D\"\ncodec=\"cii-date-102\"\n"
)]
#[case::e085_codec_on_string(
    "E085",
    "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\ncodec=\"date-iso\"\n"
)]
#[case::e087_wire_collision(
    "E087",
    "[Doc.D]\ntype=\"date\"\ncanonical_key=\"D\"\ncodec=\"cii-date-102\"\n[Doc.D.f]\nxml=\"@format\"\ntype=\"string\"\ncanonical_key=\"F\"\n"
)]
#[case::e090_two_without_selector("E090", concat!(
    "[Doc.Ref]\ntype=\"collection\"\ncanonical_key=\"Refs\"\n[Doc.Ref.ID]\ntype=\"identifier\"\ncanonical_key=\"RefId\"\n",
    "[Doc.Tender]\nxml=\"Ref\"\n[Doc.Tender.ID]\ntype=\"identifier\"\ncanonical_key=\"TenderRef\"\n"
))]
#[case::e091_match_on_scalar(
    "E091",
    "[Doc.S]\ntype=\"string\"\ncanonical_key=\"S\"\nmatch={\"X\"=\"1\"}\n"
)]
#[case::e092_undeclared_selector_key("E092", concat!(
    "[Doc.Ref]\ntype=\"collection\"\ncanonical_key=\"Refs\"\n[Doc.Ref.ID]\ntype=\"identifier\"\ncanonical_key=\"RefId\"\n",
    "[Doc.Tender]\nxml=\"Ref\"\nmatch={\"Nope\"=\"50\"}\n[Doc.Tender.ID]\ntype=\"identifier\"\ncanonical_key=\"TenderRef\"\n"
))]
#[case::e093_parent_at_root(
    "E093",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\nclone_of=\"$parent.InvoiceNumber\"\n"
)]
#[case::e093_sibling(
    "E093",
    "[Doc.ID]\ntype=\"identifier\"\ncanonical_key=\"InvoiceNumber\"\n[Doc.B]\ntype=\"identifier\"\nclone_of=\"$sibling.InvoiceNumber\"\n"
)]
fn test_rejected(#[case] code: &str, #[case] body: &str) {
    assert_rejected(&one(body), code);
}

/// Two logical nodes with identical selectors overlap.
#[rstest]
#[case::e090_identical_selectors("E090", concat!(
    "[Doc.Ref]\ntype=\"collection\"\ncanonical_key=\"Refs\"\n[Doc.Ref.ID]\ntype=\"identifier\"\ncanonical_key=\"RefId\"\n",
    "[Doc.T1]\nxml=\"Ref\"\nmatch={\"ID\"=\"50\"}\n[Doc.T1.X]\ntype=\"identifier\"\ncanonical_key=\"T1\"\n",
    "[Doc.T2]\nxml=\"Ref\"\nmatch={\"ID\"=\"50\"}\n[Doc.T2.X]\ntype=\"identifier\"\ncanonical_key=\"T2\"\n"
))]
fn test_rejected_selectors(#[case] code: &str, #[case] body: &str) {
    assert_rejected(&one(body), code);
}

/// Namespace cases need a `[meta]` with namespace tables.
#[rstest]
#[case::e080_root_ns("E080", "root_ns = \"zz\"", KEY)]
#[case::e080_ns_default("E080", "[meta.ns_defaults]\nleaf = \"zz\"", KEY)]
#[case::e081_ns_on_attribute(
    "E081",
    "[meta.namespaces]\ncbc = \"urn:cbc\"",
    "[Doc.A]\ntype=\"decimal\"\ncanonical_key=\"A\"\n[Doc.A.c]\nxml=\"@c\"\ntype=\"currency\"\ncanonical_key=\"AC\"\nns=\"cbc\"\n"
)]
#[case::e026_root_not_an_identifier("E026", "", KEY)]
fn test_rejected_meta(#[case] code: &str, #[case] extra: &str, #[case] body: &str) {
    let mut text = meta("t", extra) + body;
    if code == "E026" {
        text = text.replace("root = \"Doc\"", "root = \"my-root\"");
    }
    assert_rejected(&[("t.toml".into(), text)], code);
}

/// A failed synthesis already reports the node; validation must not add an
/// `E021` "empty source path" for it.
#[test]
fn test_synthesis_error_is_not_followed_by_an_empty_path_e021() {
    let body = "[Doc.A]\ntype=\"string\"\ncanonical_key=\"K\"\n[Doc.A.at]\nxml=\"@at\"\ntype=\"string\"\ncanonical_key=\"K2\"\nmultiple=\"first\"\n";
    match run(&one(body)) {
        Outcome::Compiled { codes, .. } => assert_eq!(codes, ["E024"]),
        Outcome::LoadError(e) => panic!("{e}"),
    }
}

#[test]
fn test_e010_cross_spoke_type_conflict() {
    let spoke = |fmt: &str, ty: &str| {
        (
            format!("{fmt}.toml"),
            meta(fmt, "") + &format!("[Doc.X]\ntype=\"{ty}\"\ncanonical_key=\"K\"\n"),
        )
    };
    assert_rejected(&[spoke("a", "date"), spoke("b", "decimal")], "E010");
    assert_rejected(&[spoke("a", "string"), spoke("b", "identifier")], "E010");
}

#[test]
fn test_w095_required_key_no_other_spoke_maps() {
    let out = run(&[
        (
            "a.toml".into(),
            meta("a", "") + "[Doc.X]\ntype=\"string\"\ncanonical_key=\"OnlyA\"\nrequired=true\n",
        ),
        ("b.toml".into(), meta("b", "") + KEY),
    ]);
    let Outcome::Compiled { codes, out, .. } = out else {
        panic!("load failed")
    };
    assert_eq!(codes, ["W095"]);
    assert!(!out.has_errors(), "W095 is a warning");
}

// --- loader refusals ----------------------------------------------------------

#[rstest]
#[case::e001_unknown_node_field(format!("{KEY}typo_field = 1\n"), "typo_field")]
#[case::e001_unknown_type("[Doc.ID]\ntype=\"amount\"\n".to_string(), "amount")]
#[case::e001_unknown_normalize("[Doc.ID]\ntype=\"string\"\nnormalize=[\"strip\"]\n".to_string(), "strip")]
#[case::e001_unknown_multiple("[Doc.ID]\ntype=\"string\"\nmultiple=\"last\"\n".to_string(), "last")]
#[case::e001_match_value_not_a_string(
    "[Doc.Ref]\ntype=\"collection\"\ncanonical_key=\"Refs\"\n[Doc.Ref.T]\ntype=\"string\"\n[Doc.R2]\nxml=\"Ref\"\nmatch={\"T\"=5}\n[Doc.R2.X]\ntype=\"string\"\ncanonical_key=\"X\"\n".to_string(),
    "match"
)]
#[case::e100_missing_xsd(format!("[meta.schema]\nxsd = \"nope.xsd\"\n{KEY}"), "E100")]
#[case::e100_absolute(format!("[meta.schema]\nxsd = \"/etc/passwd\"\n{KEY}"), "E100")]
#[case::e100_escapes_root(format!("[meta.schema]\nxsd = \"../../../../../../etc/passwd\"\n{KEY}"), "E100")]
#[case::e101_unknown_sample_source(
    format!("[[meta.samples]]\nfile = \"config/mappings/t.toml\"\nsource = \"ghost\"\n{KEY}"),
    "E101"
)]
fn test_load_error(#[case] body: String, #[case] needle: &str) {
    assert_load_error(&one(&body), needle);
}

/// An id segment named like a node field (`match`, `type`, `xml`, …) is read as
/// that field, so such an element is bound with `xml` instead; the loader
/// refuses the ambiguous form rather than guessing.
#[test]
fn test_segment_named_like_a_node_field_is_a_load_error() {
    assert_load_error(
        &one("[Doc.match.ID]\ntype=\"string\"\ncanonical_key=\"K\"\n"),
        "match",
    );
}

#[test]
fn test_e001_unknown_meta_field() {
    let text = meta("t", "bogus = 1") + KEY;
    assert_load_error(&[("t.toml".into(), text)], "bogus");
}

#[rstest]
#[case::missing_parent(&[("c", "inherits = \"ghost:1\"")], "ghost")]
#[case::cycle(&[("a", "inherits = \"b:1\""), ("b", "inherits = \"a:1\"")], "cycl")]
#[case::self_inherit(&[("a", "inherits = \"a:1\"")], "cycl")]
#[case::duplicate_ids(&[("same", ""), ("same", "")], "same")]
fn test_inheritance_load_error(#[case] spokes: &[(&str, &str)], #[case] needle: &str) {
    let files: Vec<(String, String)> = spokes
        .iter()
        .enumerate()
        .map(|(i, (fmt, extra))| (format!("m{i}.toml"), meta(fmt, extra) + KEY))
        .collect();
    assert_load_error(&files, needle);
}

#[test]
fn test_disabling_an_inherited_fallback_target_is_e030() {
    assert_rejected(
        &[
            (
                "base.toml".into(),
                meta("base", "")
                    + "[Doc.A]\ntype=\"string\"\ncanonical_key=\"A\"\nfallbacks=[\"Doc.B\"]\n[Doc.B]\ntype=\"string\"\n",
            ),
            (
                "child.toml".into(),
                meta("child", "inherits = \"base:1\"") + "[Doc.B]\ndisabled=true\n",
            ),
        ],
        "E030",
    );
}
