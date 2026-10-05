//! Configuration loader: the one way codecs and spokes are discovered and
//! each spoke's inheritance chain resolved.
//!
//! Both consumers of the compiler — the `einvoice-interfaces` build script and
//! the `xtask` dev CLI (`cargo run -p einvoice-dsl -- check|report`) — load the
//! same `config/` directory: `config/codecs/*.toml` (the shared codecs, loaded
//! first) and `config/mappings/*.toml` (the spokes). This module owns that
//! loading so the two can never diverge: scanning `*.toml`, parsing, resolving
//! each spoke's `[meta].inherits` chain (ancestor-first), skipping
//! `disabled = true` inherit-only bases, deriving each spoke's slug from
//! `[meta].doc_format`, and checking the files the mappings declare for their
//! schema conformance checks.
//!
//! # Structure
//!
//! - [`LoadedSpoke`] — one emitted spoke: its slug, owned mapping chain, and
//!   the sample documents it reads.
//! - [`LoadOutput`] — the codec table, the loaded spokes, the scanned file
//!   paths and the declared data files (the build script registers both for
//!   `rerun-if-changed`), and the workspace root.
//! - [`load_config`] — load a `config/` directory: codecs, then mappings.
//! - [`load_dir`] — scan + parse + chain-resolve one mappings directory.
//! - [`slug_of`] — `doc_format` → `snake_case` Rust module id.
//!
//! # Behavior
//!
//! Spokes are returned in slug order. Errors (unreadable dir/file, TOML parse
//! failure, duplicate codec ids, duplicate mapping ids or slugs, unknown/cyclic
//! `inherits`) are fatal [`ConfigError`]s: loading cannot proceed past them. A
//! missing `codecs/` directory simply means no codecs.
//!
//! The paths in `[meta.schema]` and `[[meta.samples]]` are relative to the
//! workspace root — for [`load_config`], the parent of the `config/` directory.
//! Every one must name an existing file (E100), and every sample must resolve
//! to the emitted spoke that reads it (E101): its `source` (a mapping id or a
//! bare `doc_format`), else the declaring mapping, which must then not be an
//! inherit-only base. These checks report every problem at once, as one
//! [`ConfigError`] with a line per problem naming the mapping file.
//!
//! # Testing
//!
//! Unit tests cover chain resolution, disabled-base skipping, slug derivation,
//! sample resolution and the declared-file checks, and each structural error
//! path, over temp directories.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::codec::{CodecTable, parse_codecs};
use crate::error::ConfigError;
use crate::parse::{ParsedMapping, parse_mapping};

/// One emitted spoke: its meta-derived slug, its inheritance chain
/// (ancestor-first, leaf-last), and the sample documents it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSpoke {
    /// `snake_case` module id derived from `[meta].doc_format` (e.g.
    /// `ubl_invoice`). Keys the compile output and names the generated module.
    pub slug: String,
    /// The mapping chain, ancestor-first and leaf-last.
    pub chain: Vec<ParsedMapping>,
    /// The `[[meta.samples]]` files this spoke reads, workspace-relative as
    /// declared: its own samples without a `source`, and those any mapping
    /// declares with a `source` naming it. Mappings in id order, each one's
    /// samples in declaration order, duplicates dropped.
    pub samples: Vec<String>,
}

/// The result of loading a configuration (or a bare mappings directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutput {
    /// The shared codecs by id (empty for [`load_dir`]).
    pub codecs: CodecTable,
    /// Emitted spokes in slug order (disabled inherit-only bases excluded).
    pub spokes: Vec<LoadedSpoke>,
    /// Every `*.toml` file scanned, sorted (for build-script change tracking).
    pub files: Vec<PathBuf>,
    /// Every file a `[meta.schema]` or `[[meta.samples]]` entry declares,
    /// resolved under [`Self::root`], sorted and de-duplicated (for
    /// build-script change tracking).
    pub declared_files: Vec<PathBuf>,
    /// The workspace root the declared paths are relative to.
    pub root: PathBuf,
}

/// Loads a `config/` directory: the codecs from `<dir>/codecs/*.toml` (if the
/// directory exists), then the spoke mappings from `<dir>/mappings/*.toml` via
/// [`load_dir`], with the parent of `dir` as the workspace root.
///
/// # Errors
///
/// Everything [`load_dir`] fails on, plus a `dir` that cannot be resolved, an
/// unreadable codecs directory or file, an invalid codec file, or a codec id
/// declared in more than one file.
pub fn load_config(dir: &Path) -> Result<LoadOutput, ConfigError> {
    // Canonical first, so `config`, `./config` and `.` all find their parent.
    let root = std::fs::canonicalize(dir)
        .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", dir.display())))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            ConfigError::msg(format!(
                "`{}` has no parent directory to serve as the workspace root",
                dir.display()
            ))
        })?;
    let codecs_dir = dir.join("codecs");
    let mut codecs = CodecTable::new();
    let mut codec_files: Vec<PathBuf> = Vec::new();
    if codecs_dir.is_dir() {
        codec_files = toml_files(&codecs_dir)?;
        for path in &codec_files {
            let src = std::fs::read_to_string(path)
                .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", path.display())))?;
            let parsed = parse_codecs(&src)
                .map_err(|e| ConfigError::msg(format!("{}: {}", path.display(), e.message)))?;
            for codec in parsed {
                if codecs.contains_key(&codec.id) {
                    return Err(ConfigError::msg(format!(
                        "{}: codec `{}` is already declared in another file",
                        path.display(),
                        codec.id
                    )));
                }
                codecs.insert(codec.id.clone(), codec);
            }
        }
    }
    let mappings = load_dir(&dir.join("mappings"), &root)?;
    let mut files = codec_files;
    files.extend(mappings.files);
    Ok(LoadOutput {
        codecs,
        files,
        ..mappings
    })
}

/// The sorted `*.toml` files directly inside `dir`.
fn toml_files(dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", dir.display())))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", dir.display())))?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    Ok(files)
}

/// Loads every spoke mapping from `dir`: scans `*.toml`, parses each file,
/// resolves `inherits` chains, skips `disabled` inherit-only bases, derives
/// slugs from `[meta].doc_format`, and checks the declared schema and sample
/// files under the workspace `root`. The returned codec table is empty.
///
/// # Errors
///
/// Fails on an unreadable directory or file, a TOML parse error, two mappings
/// sharing an id or slug, an `inherits` reference to an unknown mapping, or an
/// inheritance cycle. Then, all reported together: a declared schema or
/// sample path that is absolute or names no file under `root` (E100), and a
/// sample no emitted spoke reads (E101).
pub fn load_dir(dir: &Path, root: &Path) -> Result<LoadOutput, ConfigError> {
    let files = toml_files(dir)?;

    // Parse every mapping first, keyed by its mapping id, so a spoke's
    // `inherits` can resolve to an ancestor regardless of file order. The
    // file each came from names it in the declared-file diagnostics.
    let mut by_id: BTreeMap<String, ParsedMapping> = BTreeMap::new();
    let mut file_of: BTreeMap<String, &Path> = BTreeMap::new();
    for path in &files {
        let src = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", path.display())))?;
        let mapping = parse_mapping(&src)
            .map_err(|e| ConfigError::msg(format!("{}: {}", path.display(), e.message)))?;
        let id = mapping_id(&mapping);
        if by_id.insert(id.clone(), mapping).is_some() {
            return Err(ConfigError::msg(format!(
                "two spokes share the same mapping id `{id}`"
            )));
        }
        file_of.insert(id, path);
    }
    if by_id.is_empty() {
        return Err(ConfigError::msg(format!(
            "no `*.toml` spokes found in `{}`",
            dir.display()
        )));
    }

    // Assemble each spoke's ancestor-first chain by following `inherits`. A
    // disabled mapping stays in `by_id` as a resolvable parent but emits no
    // spoke of its own (inherit-only base syntax).
    let mut spokes: Vec<LoadedSpoke> = Vec::new();
    for (id, mapping) in &by_id {
        if mapping.meta.disabled {
            continue;
        }
        let slug = slug_of(&mapping.meta.doc_format)?;
        if spokes.iter().any(|s| s.slug == slug) {
            return Err(ConfigError::msg(format!(
                "two spokes derive the same name `{slug}` from doc_format `{}`",
                mapping.meta.doc_format
            )));
        }
        spokes.push(LoadedSpoke {
            slug,
            chain: resolve_chain(id, &by_id)?,
            samples: Vec::new(),
        });
    }
    spokes.sort_by(|a, b| a.slug.cmp(&b.slug));

    // The conformance declarations: every declared file must exist, every
    // sample must have a reader. All problems are reported together.
    let mut problems: Vec<String> = Vec::new();
    let mut declared_files: Vec<PathBuf> = Vec::new();
    for (id, mapping) in &by_id {
        let file = file_of[id].display();
        let meta = &mapping.meta;
        let mut declared: Vec<(&str, &str)> = Vec::new();
        if let Some(schema) = &meta.schema {
            declared.push(("[meta.schema].xsd", &schema.xsd));
            if let Some(catalog) = &schema.catalog {
                declared.push(("[meta.schema].catalog", catalog));
            }
        }
        for sample in &meta.samples {
            declared.push(("[[meta.samples]].file", &sample.file));
        }
        for (field, path) in declared {
            match declared_file(root, path) {
                Ok(resolved) => declared_files.push(resolved),
                Err(why) => problems.push(format!("{file}: E100: {field} `{path}` {why}")),
            }
        }

        for sample in &meta.samples {
            let reader = match &sample.source {
                Some(source) => emitted_spoke(source, &by_id).ok_or_else(|| {
                    format!(
                        "sample `{}` names source `{source}`, which is no emitted spoke (a mapping id or a `doc_format` of a mapping that is not `disabled`)",
                        sample.file
                    )
                }),
                None if meta.disabled => Err(format!(
                    "sample `{}` is declared on an inherit-only base, which reads nothing: name the spoke that reads it with `source`",
                    sample.file
                )),
                None => slug_of(&meta.doc_format).map_err(|e| e.message),
            };
            match reader {
                Ok(slug) => {
                    let spoke = spokes
                        .iter_mut()
                        .find(|s| s.slug == slug)
                        .expect("an emitted mapping's slug names a loaded spoke");
                    if !spoke.samples.contains(&sample.file) {
                        spoke.samples.push(sample.file.clone());
                    }
                }
                Err(why) => problems.push(format!("{file}: E101: {why}")),
            }
        }
    }
    if !problems.is_empty() {
        return Err(ConfigError::msg(problems.join("\n")));
    }
    declared_files.sort();
    declared_files.dedup();

    Ok(LoadOutput {
        codecs: CodecTable::new(),
        spokes,
        files,
        declared_files,
        root: root.to_path_buf(),
    })
}

/// Resolves a declared workspace-relative `path` under `root`, or says why it
/// cannot be used: it is absolute, climbs out of `root` with `..`, or no file
/// exists there.
fn declared_file(root: &Path, path: &str) -> Result<PathBuf, String> {
    let rel = Path::new(path);
    if rel.is_absolute() || rel.has_root() {
        return Err("must be relative to the workspace root, not absolute".to_string());
    }
    if rel.components().any(|c| c == Component::ParentDir) {
        return Err("must stay under the workspace root (no `..` components)".to_string());
    }
    let resolved = root.join(path);
    if resolved.is_file() {
        Ok(resolved)
    } else {
        Err(format!(
            "does not exist (paths are relative to the workspace root `{}`)",
            root.display()
        ))
    }
}

/// The slug of the emitted (not `disabled`) mapping `name` identifies: its
/// mapping id, or its bare `doc_format`.
fn emitted_spoke(name: &str, by_id: &BTreeMap<String, ParsedMapping>) -> Option<String> {
    by_id
        .iter()
        .filter(|(_, m)| !m.meta.disabled)
        .find(|(id, m)| id.as_str() == name || m.meta.doc_format == name)
        .and_then(|(_, m)| slug_of(&m.meta.doc_format).ok())
}

/// A mapping's identity, mirroring `build_ir`'s `source_model` fallback: the
/// explicit `[meta].source_model`, else `<doc_format>:<format_version>`. This is
/// the id an `inherits` field references.
pub fn mapping_id(mapping: &ParsedMapping) -> String {
    let meta = &mapping.meta;
    meta.source_model
        .clone()
        .unwrap_or_else(|| format!("{}:{}", meta.doc_format, meta.format_version))
}

/// Follows `leaf`'s `inherits` links to build its chain ancestor-first,
/// leaf-last. Errors on a missing ancestor or an inheritance cycle.
fn resolve_chain(
    leaf: &str,
    by_id: &BTreeMap<String, ParsedMapping>,
) -> Result<Vec<ParsedMapping>, ConfigError> {
    let mut ids: Vec<String> = Vec::new();
    let mut cur = leaf.to_string();
    loop {
        if ids.contains(&cur) {
            return Err(ConfigError::msg(format!(
                "inheritance cycle through mapping id `{cur}`"
            )));
        }
        let mapping = by_id.get(&cur).ok_or_else(|| {
            ConfigError::msg(format!("mapping `{leaf}` inherits unknown parent `{cur}`"))
        })?;
        ids.push(cur.clone());
        match &mapping.meta.inherits {
            Some(parent) => cur = parent.clone(),
            None => break,
        }
    }
    Ok(ids.iter().rev().map(|id| by_id[id].clone()).collect())
}

/// `snake_case` Rust module id from a meta `doc_format` (e.g. `ubl-invoice` →
/// `ubl_invoice`). Any run of non-alphanumerics collapses to a single `_`.
///
/// # Errors
///
/// Fails when the result is not a valid Rust identifier (empty, or starting
/// with a digit).
pub fn slug_of(doc_format: &str) -> Result<String, ConfigError> {
    let mut out = String::new();
    let mut prev_us = false;
    for c in doc_format.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    let slug = out.trim_matches('_').to_string();
    if slug.is_empty() || slug.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(ConfigError::msg(format!(
            "doc_format `{doc_format}` does not yield a valid Rust identifier"
        )));
    }
    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `files` as `<name>.toml` into a fresh temp dir and returns it.
    fn dir_with(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "einvoice-loader-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        for (name, body) in files {
            std::fs::write(dir.join(format!("{name}.toml")), body).expect("write mapping");
        }
        dir
    }

    fn meta(doc_format: &str, extra: &str) -> String {
        format!(
            r#"
            [meta]
            doc_format = "{doc_format}"
            format_version = "1"
            mapping_version = "1"
            canonical_model = "c:1"
            root = "Doc"
            {extra}

            [Doc.ID]
            type = "identifier"
            canonical_key = "InvoiceNumber"
        "#
        )
    }

    #[test]
    fn test_load_dir_resolves_inheritance_chain() {
        let dir = dir_with(&[
            ("base", &meta("base-fmt", "")),
            ("child", &meta("child-fmt", r#"inherits = "base-fmt:1""#)),
        ]);
        let out = load_dir(&dir, &dir).expect("loads");
        assert_eq!(out.spokes.len(), 2);
        let child = out
            .spokes
            .iter()
            .find(|s| s.slug == "child_fmt")
            .expect("child spoke");
        assert_eq!(child.chain.len(), 2, "ancestor-first chain");
        assert_eq!(child.chain[0].meta.doc_format, "base-fmt");
        assert_eq!(child.chain[1].meta.doc_format, "child-fmt");
    }

    #[test]
    fn test_load_dir_skips_disabled_base_but_resolves_it() {
        let dir = dir_with(&[
            ("base", &meta("base-fmt", "disabled = true")),
            ("child", &meta("child-fmt", r#"inherits = "base-fmt:1""#)),
        ]);
        let out = load_dir(&dir, &dir).expect("loads");
        // The disabled base emits no spoke but still parents the child's chain.
        assert_eq!(out.spokes.len(), 1);
        assert_eq!(out.spokes[0].slug, "child_fmt");
        assert_eq!(out.spokes[0].chain.len(), 2);
    }

    #[test]
    fn test_load_dir_unknown_parent_is_error() {
        let dir = dir_with(&[("child", &meta("child-fmt", r#"inherits = "ghost:1""#))]);
        let err = load_dir(&dir, &dir).unwrap_err();
        assert!(err.message.contains("unknown parent"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_inheritance_cycle_is_error() {
        let dir = dir_with(&[
            ("a", &meta("a-fmt", r#"inherits = "b-fmt:1""#)),
            ("b", &meta("b-fmt", r#"inherits = "a-fmt:1""#)),
        ]);
        let err = load_dir(&dir, &dir).unwrap_err();
        assert!(err.message.contains("cycle"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_duplicate_mapping_id_is_error() {
        let dir = dir_with(&[("a", &meta("same-fmt", "")), ("b", &meta("same-fmt", ""))]);
        let err = load_dir(&dir, &dir).unwrap_err();
        assert!(err.message.contains("same mapping id"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_empty_dir_is_error() {
        let dir = dir_with(&[]);
        let err = load_dir(&dir, &dir).unwrap_err();
        assert!(err.message.contains("no `*.toml`"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_lists_scanned_files_sorted() {
        let dir = dir_with(&[("b", &meta("b-fmt", "")), ("a", &meta("a-fmt", ""))]);
        let out = load_dir(&dir, &dir).expect("loads");
        let names: Vec<_> = out
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["a.toml", "b.toml"]);
    }

    /// A `config/` layout: `codecs/*.toml` and `mappings/*.toml` under one root.
    fn config_with(codecs: &[(&str, &str)], mappings: &[(&str, &str)]) -> PathBuf {
        let dir = dir_with(&[]);
        std::fs::create_dir_all(dir.join("codecs")).unwrap();
        std::fs::create_dir_all(dir.join("mappings")).unwrap();
        for (name, body) in codecs {
            std::fs::write(dir.join("codecs").join(format!("{name}.toml")), body).unwrap();
        }
        for (name, body) in mappings {
            std::fs::write(dir.join("mappings").join(format!("{name}.toml")), body).unwrap();
        }
        dir
    }

    const DATE_CODEC: &str = "[codec.cii-date-102]\nfor_type = \"date\"\nlexical = \"YYYYMMDD\"\nwire = { \"@format\" = \"102\" }\n";

    #[test]
    fn test_load_config_loads_codecs_then_mappings() {
        let dir = config_with(&[("dates", DATE_CODEC)], &[("base", &meta("base-fmt", ""))]);
        let out = load_config(&dir).expect("loads");
        assert_eq!(out.codecs.len(), 1);
        assert_eq!(out.codecs["cii-date-102"].wire["format"], "102");
        assert_eq!(out.spokes.len(), 1);
        let names: Vec<_> = out
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["dates.toml", "base.toml"], "codec files first");
    }

    #[test]
    fn test_load_config_without_codecs_dir_has_no_codecs() {
        let dir = dir_with(&[]);
        std::fs::create_dir_all(dir.join("mappings")).unwrap();
        std::fs::write(dir.join("mappings/base.toml"), meta("base-fmt", "")).unwrap();
        let out = load_config(&dir).expect("loads");
        assert!(out.codecs.is_empty());
        assert_eq!(out.spokes.len(), 1);
    }

    #[test]
    fn test_load_config_duplicate_codec_id_across_files_is_error() {
        let dir = config_with(
            &[("a", DATE_CODEC), ("b", DATE_CODEC)],
            &[("base", &meta("base-fmt", ""))],
        );
        let err = load_config(&dir).unwrap_err();
        assert!(err.message.contains("already declared"), "{}", err.message);
    }

    #[test]
    fn test_load_config_invalid_codec_names_the_file() {
        let dir = config_with(
            &[("bad", "[codec.x]\nfor_type = \"date\"\nlexical = \"YYYY\"")],
            &[("base", &meta("base-fmt", ""))],
        );
        let err = load_config(&dir).unwrap_err();
        assert!(err.message.contains("bad.toml"), "{}", err.message);
        assert!(err.message.contains("`MM` is required"), "{}", err.message);
    }

    /// Writes `body` to `<dir>/<rel>`, creating its parent directories.
    fn touch(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn test_load_dir_attributes_samples_to_their_reader_and_lists_declared_files() {
        let dir = dir_with(&[
            (
                "base",
                &meta(
                    "base-fmt",
                    "[meta.schema]\nxsd = \"xsd/base.xsd\"\ncatalog = \"xsd/catalog.xml\"\n[[meta.samples]]\nfile = \"docs/base.xml\"\n[[meta.samples]]\nfile = \"docs/child.xml\"\nsource = \"child-fmt\"",
                ),
            ),
            (
                "child",
                &meta(
                    "child-fmt",
                    "inherits = \"base-fmt:1\"\n[[meta.samples]]\nfile = \"docs/child.xml\"\n[[meta.samples]]\nfile = \"docs/other.xml\"",
                ),
            ),
        ]);
        for rel in [
            "xsd/base.xsd",
            "xsd/catalog.xml",
            "docs/base.xml",
            "docs/child.xml",
            "docs/other.xml",
        ] {
            touch(&dir, rel, "<x/>");
        }
        let out = load_dir(&dir, &dir).expect("loads");
        assert_eq!(out.root, dir);
        let samples = |slug: &str| {
            out.spokes
                .iter()
                .find(|s| s.slug == slug)
                .expect("spoke")
                .samples
                .clone()
        };
        assert_eq!(samples("base_fmt"), ["docs/base.xml"]);
        assert_eq!(
            samples("child_fmt"),
            ["docs/child.xml", "docs/other.xml"],
            "`source` attributes a sample to its reader; a duplicate is dropped"
        );
        let declared: Vec<_> = out
            .declared_files
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            declared,
            [
                "docs/base.xml",
                "docs/child.xml",
                "docs/other.xml",
                "xsd/base.xsd",
                "xsd/catalog.xml"
            ],
            "sorted, de-duplicated, resolved under the root"
        );
    }

    #[test]
    fn test_load_dir_sample_source_may_be_a_mapping_id_on_an_inherit_only_base() {
        let dir = dir_with(&[
            (
                "base",
                &meta(
                    "base-fmt",
                    "disabled = true\n[[meta.samples]]\nfile = \"doc.xml\"\nsource = \"child-fmt:1\"",
                ),
            ),
            ("child", &meta("child-fmt", r#"inherits = "base-fmt:1""#)),
        ]);
        touch(&dir, "doc.xml", "<x/>");
        let out = load_dir(&dir, &dir).expect("loads");
        assert_eq!(out.spokes.len(), 1);
        assert_eq!(out.spokes[0].samples, ["doc.xml"]);
    }

    #[test]
    fn test_load_dir_missing_or_absolute_declared_file_is_e100_naming_the_mapping() {
        let dir = dir_with(&[(
            "base",
            &meta(
                "base-fmt",
                "[meta.schema]\nxsd = \"xsd/missing.xsd\"\ncatalog = \"/abs/catalog.xml\"\n[[meta.samples]]\nfile = \"docs/missing.xml\"",
            ),
        )]);
        let err = load_dir(&dir, &dir).unwrap_err();
        let lines: Vec<&str> = err.message.lines().collect();
        assert_eq!(lines.len(), 3, "every problem at once: {}", err.message);
        for line in &lines {
            assert!(line.contains("base.toml: E100:"), "{line}");
        }
        assert!(
            lines[0].contains("[meta.schema].xsd `xsd/missing.xsd` does not exist"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("not absolute"), "{}", lines[1]);
        assert!(lines[2].contains("[[meta.samples]].file"), "{}", lines[2]);
    }

    #[test]
    fn test_load_dir_declared_file_escaping_the_root_is_e100() {
        // The file exists (it is the mapping itself), but `..` reaches it from
        // outside the root: still E100, so a declaration never leaves the workspace.
        let dir = dir_with(&[(
            "base",
            &meta("base-fmt", "[meta.schema]\nxsd = \"../x/../base.toml\""),
        )]);
        let err = load_dir(&dir, &dir).unwrap_err();
        assert!(err.message.contains("base.toml: E100:"), "{}", err.message);
        assert!(err.message.contains("no `..`"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_sample_without_a_reader_is_e101() {
        let dir = dir_with(&[
            (
                "base",
                &meta(
                    "base-fmt",
                    "disabled = true\n[[meta.samples]]\nfile = \"doc.xml\"",
                ),
            ),
            (
                "child",
                &meta(
                    "child-fmt",
                    "inherits = \"base-fmt:1\"\n[[meta.samples]]\nfile = \"doc.xml\"\nsource = \"ghost-fmt\"\n[[meta.samples]]\nfile = \"doc.xml\"\nsource = \"base-fmt\"",
                ),
            ),
        ]);
        touch(&dir, "doc.xml", "<x/>");
        let err = load_dir(&dir, &dir).unwrap_err();
        let lines: Vec<&str> = err.message.lines().collect();
        assert_eq!(lines.len(), 3, "{}", err.message);
        assert!(
            lines[0].contains("base.toml: E101:") && lines[0].contains("inherit-only base"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("child.toml: E101:") && lines[1].contains("`ghost-fmt`"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("`base-fmt`"),
            "a disabled base reads nothing: {}",
            lines[2]
        );
    }

    #[test]
    fn test_load_config_resolves_declared_paths_under_the_parent_of_the_config_dir() {
        let workspace = dir_with(&[]);
        let config = workspace.join("config");
        std::fs::create_dir_all(config.join("mappings")).unwrap();
        std::fs::write(
            config.join("mappings/base.toml"),
            meta("base-fmt", "[meta.schema]\nxsd = \"testfiles/base.xsd\""),
        )
        .unwrap();
        touch(&workspace, "testfiles/base.xsd", "<xs:schema/>");
        let out = load_config(&config).expect("loads");
        assert_eq!(out.root, workspace.canonicalize().unwrap());
        assert_eq!(
            out.declared_files,
            [workspace.canonicalize().unwrap().join("testfiles/base.xsd")]
        );
    }

    #[test]
    fn test_slug_of_collapses_non_alphanumerics() {
        assert_eq!(slug_of("ubl-invoice").unwrap(), "ubl_invoice");
        assert_eq!(slug_of("Factur--X!!v1").unwrap(), "factur_x_v1");
    }

    #[test]
    fn test_slug_of_invalid_identifier_is_error() {
        assert!(slug_of("---").is_err());
        assert!(slug_of("1abc").is_err());
    }
}
