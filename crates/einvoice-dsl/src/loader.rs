//! Mappings-directory loader: the one way spokes are discovered and their
//! inheritance chains resolved.
//!
//! Both consumers of the compiler — the `einvoice-interfaces` build script and
//! the `xtask` dev CLI (`cargo run -p einvoice-dsl -- check|report`) — load the
//! same `mappings/` directory. This module owns that loading so the two can
//! never diverge: scanning `*.toml`, parsing, resolving each spoke's
//! `[meta].inherits` chain (ancestor-first), skipping `disabled = true`
//! inherit-only bases, and deriving each spoke's slug from `[meta].doc_format`.
//!
//! # Structure
//!
//! - [`LoadedSpoke`] — one emitted spoke: its slug and owned mapping chain.
//! - [`LoadOutput`] — the loaded spokes plus the scanned file paths (the build
//!   script registers those for `rerun-if-changed`).
//! - [`load_dir`] — scan + parse + chain-resolve one directory.
//! - [`slug_of`] — `doc_format` → `snake_case` Rust module id.
//!
//! # Behavior
//!
//! Spokes are returned in slug order. Errors (unreadable dir/file, TOML parse
//! failure, duplicate mapping ids or slugs, unknown/cyclic `inherits`) are
//! fatal [`ConfigError`]s: loading cannot proceed past them.
//!
//! # Testing
//!
//! Unit tests cover chain resolution, disabled-base skipping, slug derivation,
//! and each structural error path, over temp directories.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::parse::{ParsedMapping, parse_mapping};

/// One emitted spoke: its meta-derived slug and its inheritance chain
/// (ancestor-first, leaf-last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSpoke {
    /// `snake_case` module id derived from `[meta].doc_format` (e.g.
    /// `ubl_invoice`). Keys the compile output and names the generated module.
    pub slug: String,
    /// The mapping chain, ancestor-first and leaf-last.
    pub chain: Vec<ParsedMapping>,
}

/// The result of loading a mappings directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutput {
    /// Emitted spokes in slug order (disabled inherit-only bases excluded).
    pub spokes: Vec<LoadedSpoke>,
    /// Every `*.toml` file scanned, sorted (for build-script change tracking).
    pub files: Vec<PathBuf>,
}

/// Loads every spoke mapping from `dir`: scans `*.toml`, parses each file,
/// resolves `inherits` chains, skips `disabled` inherit-only bases, and derives
/// slugs from `[meta].doc_format`.
///
/// # Errors
///
/// Fails on an unreadable directory or file, a TOML parse error, two mappings
/// sharing an id or slug, an `inherits` reference to an unknown mapping, or an
/// inheritance cycle.
pub fn load_dir(dir: &Path) -> Result<LoadOutput, ConfigError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", dir.display())))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ConfigError::msg(format!("cannot read `{}`: {e}", dir.display())))?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();

    // Parse every mapping first, keyed by its mapping id, so a spoke's
    // `inherits` can resolve to an ancestor regardless of file order.
    let mut by_id: BTreeMap<String, ParsedMapping> = BTreeMap::new();
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
        });
    }
    spokes.sort_by(|a, b| a.slug.cmp(&b.slug));

    Ok(LoadOutput { spokes, files })
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
        let out = load_dir(&dir).expect("loads");
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
        let out = load_dir(&dir).expect("loads");
        // The disabled base emits no spoke but still parents the child's chain.
        assert_eq!(out.spokes.len(), 1);
        assert_eq!(out.spokes[0].slug, "child_fmt");
        assert_eq!(out.spokes[0].chain.len(), 2);
    }

    #[test]
    fn test_load_dir_unknown_parent_is_error() {
        let dir = dir_with(&[("child", &meta("child-fmt", r#"inherits = "ghost:1""#))]);
        let err = load_dir(&dir).unwrap_err();
        assert!(err.message.contains("unknown parent"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_inheritance_cycle_is_error() {
        let dir = dir_with(&[
            ("a", &meta("a-fmt", r#"inherits = "b-fmt:1""#)),
            ("b", &meta("b-fmt", r#"inherits = "a-fmt:1""#)),
        ]);
        let err = load_dir(&dir).unwrap_err();
        assert!(err.message.contains("cycle"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_duplicate_mapping_id_is_error() {
        let dir = dir_with(&[
            ("a", &meta("same-fmt", "")),
            ("b", &meta("same-fmt", "")),
        ]);
        let err = load_dir(&dir).unwrap_err();
        assert!(err.message.contains("same mapping id"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_empty_dir_is_error() {
        let dir = dir_with(&[]);
        let err = load_dir(&dir).unwrap_err();
        assert!(err.message.contains("no `*.toml`"), "{}", err.message);
    }

    #[test]
    fn test_load_dir_lists_scanned_files_sorted() {
        let dir = dir_with(&[("b", &meta("b-fmt", "")), ("a", &meta("a-fmt", ""))]);
        let out = load_dir(&dir).expect("loads");
        let names: Vec<_> = out
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["a.toml", "b.toml"]);
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
