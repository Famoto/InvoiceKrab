//! Emission of `MappingDiagnostic` construction snippets into generated code.

use std::fmt::Write as _;

/// A diagnostic-construction snippet to emit into generated code.
///
/// Build with [`DiagSpec::new`] (the four always-present fields), chain the
/// optional provenance setters, then call [`DiagSpec::emit`] to render the
/// `MappingDiagnostic::new(...)` construction, its field assignments, and the
/// `diagnostics.push(diag)`. `msg` is a Rust expression — a string literal or a
/// `format!` call.
pub(super) struct DiagSpec<'a> {
    severity: &'a str,
    code: &'a str,
    node_id: &'a str,
    msg: &'a str,
    canonical_key: Option<&'a str>,
    source_path: Option<&'a str>,
    index_var: Option<&'a str>,
    fallback_chain: Option<&'a str>,
}

impl<'a> DiagSpec<'a> {
    /// A diagnostic with the four required fields and no provenance.
    pub(super) fn new(severity: &'a str, code: &'a str, node_id: &'a str, msg: &'a str) -> Self {
        Self {
            severity,
            code,
            node_id,
            msg,
            canonical_key: None,
            source_path: None,
            index_var: None,
            fallback_chain: None,
        }
    }

    /// Sets the canonical key the diagnostic is about.
    pub(super) fn key(mut self, key: &'a str) -> Self {
        self.canonical_key = Some(key);
        self
    }

    /// Sets the source path the diagnostic is about.
    pub(super) fn path(mut self, path: &'a str) -> Self {
        self.source_path = Some(path);
        self
    }

    /// Sets the collection-index variable (a no-op when `None`).
    pub(super) fn index(mut self, index_var: Option<&'a str>) -> Self {
        self.index_var = index_var;
        self
    }

    /// Sets the fallback-chain `vec![...]` expression.
    pub(super) fn chain(mut self, chain: &'a str) -> Self {
        self.fallback_chain = Some(chain);
        self
    }

    /// Renders the diagnostic construction + push, indented by `pad`.
    pub(super) fn emit(&self, out: &mut String, pad: &str) {
        let Self {
            severity,
            code,
            node_id,
            msg,
            ..
        } = self;
        let _ = writeln!(
            out,
            "{pad}let mut diag = MappingDiagnostic::new({severity}, {code:?}, {node_id:?}, {msg});"
        );
        if let Some(key) = self.canonical_key {
            let _ = writeln!(out, "{pad}diag.canonical_key = Some({key:?}.to_string());");
        }
        if let Some(path) = self.source_path {
            let _ = writeln!(out, "{pad}diag.source_path = Some({path:?}.to_string());");
        }
        if let Some(idx) = self.index_var {
            let _ = writeln!(out, "{pad}diag.collection_index = Some({idx});");
        }
        if let Some(chain) = self.fallback_chain {
            let _ = writeln!(out, "{pad}diag.fallback_chain = {chain};");
        }
        let _ = writeln!(out, "{pad}diagnostics.push(diag);");
    }
}
