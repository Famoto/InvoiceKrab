//! Source document identity: what a document must declare to be read by a spoke.
//!
//! Every spoke carries an [`Identity`] generated from its mapping: the root
//! element's namespace URI (`[meta.namespaces]` at `root_ns`) and local name
//! (`[meta].root`), and the `[meta.identity]` table — the exact profile
//! (specification) identifiers it supports, the versions it supports, and the
//! mandatory identity attributes of the root. [`Identity::check`] verifies a
//! document against all of them before it is deserialized, on every read
//! ([`Engine::to_hub`](crate::Engine::to_hub)) — whether the source format was
//! auto-detected or named explicitly — and auto-detection
//! ([`detect_source`](crate::cli::detect_source)) picks the one spoke whose
//! identity a document satisfies.
//!
//! Every comparison is exact: namespaces are resolved (a prefix means nothing,
//! its URI everything), and identifier and attribute values are compared whole
//! after trimming surrounding whitespace — no substrings, no case folding.
//!
//! The identity elements are read from the document's header. Scanning stops at
//! the first top-level element after the profile element that lies on no
//! identity path (in every supported syntax the identifiers precede the
//! content), so checking a valid document costs a few events, not a pass over
//! it.

use std::fmt;

use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;

/// What a document must declare to be read by a spoke. Generated from the
/// spoke's mapping; see [`Spoke::identity`](crate::Spoke::identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// Namespace URI of the root element; `None` when the root is in no
    /// namespace.
    pub namespace: Option<&'static str>,
    /// Local name of the root element.
    pub root: &'static str,
    /// Dotted element path, under the root, of the mandatory profile
    /// identifier (BT-24); `None` when the format declares none.
    pub profile: Option<&'static str>,
    /// The exact profile identifiers supported.
    pub profiles: &'static [&'static str],
    /// Dotted element path, under the root, of an optional version element.
    pub version: Option<&'static str>,
    /// The exact version values supported.
    pub versions: &'static [&'static str],
    /// Mandatory unqualified root attributes and the exact values each may take.
    pub attributes: &'static [(&'static str, &'static [&'static str])],
}

/// Why a document does not have a spoke's identity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The bytes hold no root element, or are not well-formed up to the point
    /// the identity is read from.
    #[error("not an XML document: {0}")]
    NotXml(String),
    /// The root element's namespace URI or local name is not the format's.
    #[error("the root element is {found}, expected {expected}")]
    Root {
        /// The expected root, as `{namespace}local`.
        expected: String,
        /// The document's root, as `{namespace}local`.
        found: String,
    },
    /// The mandatory profile identifier element is absent.
    #[error("the document declares no profile identifier (<{path}>); supported: {}", list(.supported))]
    ProfileMissing {
        /// The element's path under the root.
        path: String,
        /// The supported identifiers.
        supported: Vec<String>,
    },
    /// The profile identifier is not one the format supports.
    #[error("unsupported profile identifier {found:?} in <{path}>; supported: {}", list(.supported))]
    ProfileUnsupported {
        /// The element's path under the root.
        path: String,
        /// The document's identifier.
        found: String,
        /// The supported identifiers.
        supported: Vec<String>,
    },
    /// The version element names a version the format does not support.
    #[error("unsupported version {found:?} in <{path}>; supported: {}", list(.supported))]
    VersionUnsupported {
        /// The element's path under the root.
        path: String,
        /// The document's version.
        found: String,
        /// The supported versions.
        supported: Vec<String>,
    },
    /// A mandatory root attribute is absent.
    #[error("the root element lacks the mandatory attribute `{name}`; supported: {}", list(.supported))]
    AttributeMissing {
        /// The attribute's name.
        name: String,
        /// Its supported values.
        supported: Vec<String>,
    },
    /// A mandatory root attribute has a value the format does not support.
    #[error("unsupported value {found:?} of the root attribute `{name}`; supported: {}", list(.supported))]
    AttributeUnsupported {
        /// The attribute's name.
        name: String,
        /// The document's value.
        found: String,
        /// Its supported values.
        supported: Vec<String>,
    },
}

/// `values` quoted and comma-separated, for error messages.
fn list(values: &[String]) -> String {
    values
        .iter()
        .map(|v| format!("{v:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn owned(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

/// A qualified name in Clark notation (`{urn:ns}local`, or `local` with no
/// namespace).
struct Clark<'a>(Option<&'a str>, &'a str);

impl fmt::Display for Clark<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(ns) => write!(f, "<{{{ns}}}{}>", self.1),
            None => write!(f, "<{}> (in no namespace)", self.1),
        }
    }
}

/// One identity element being looked for: its path and what it found.
struct Wanted {
    path: Vec<&'static str>,
    found: Option<String>,
}

impl Identity {
    /// The root element as `{namespace}local`, for messages.
    pub fn qualified_root(&self) -> String {
        Clark(self.namespace, self.root).to_string()
    }

    /// Verifies that `bytes` is a document of this identity: its root
    /// element's namespace URI and local name, its mandatory root attributes,
    /// its profile identifier and (when present) its version.
    ///
    /// # Errors
    ///
    /// Returns the first [`IdentityError`] found, in that order.
    pub fn check(&self, bytes: &[u8]) -> Result<(), IdentityError> {
        let mut reader = NsReader::from_reader(bytes);
        let mut buf = Vec::new();

        // The root element: its resolved namespace, local name and attributes.
        let empty_root = loop {
            let (ns, event) = reader
                .read_resolved_event_into(&mut buf)
                .map_err(|e| IdentityError::NotXml(e.to_string()))?;
            let (start, empty) = match event {
                Event::Start(e) => (e, false),
                Event::Empty(e) => (e, true),
                Event::Eof => return Err(IdentityError::NotXml("no root element".into())),
                _ => continue,
            };
            let namespace = match ns {
                ResolveResult::Bound(ns) => Some(ns.as_ref().to_owned()),
                ResolveResult::Unbound => None,
                ResolveResult::Unknown(prefix) => {
                    return Err(IdentityError::NotXml(format!(
                        "the root element's prefix `{prefix}` is bound to no namespace"
                    )));
                }
            };
            let local = start.local_name().as_ref().to_owned();
            if namespace.as_deref() != self.namespace || local != self.root {
                return Err(IdentityError::Root {
                    expected: self.qualified_root(),
                    found: Clark(namespace.as_deref(), &local).to_string(),
                });
            }
            self.check_attributes(&start)?;
            break empty;
        };

        let mut wanted = [self.profile, self.version].map(|path| Wanted {
            path: path.map(|p| p.split('.').collect()).unwrap_or_default(),
            found: None,
        });
        if !empty_root && wanted.iter().any(|w| !w.path.is_empty()) {
            self.scan_header(&mut reader, &mut buf, &mut wanted)?;
        }

        let [profile, version] = wanted;
        if let Some(path) = self.profile {
            match profile.found {
                None => {
                    return Err(IdentityError::ProfileMissing {
                        path: path.to_string(),
                        supported: owned(self.profiles),
                    });
                }
                Some(found) if !self.profiles.contains(&found.as_str()) => {
                    return Err(IdentityError::ProfileUnsupported {
                        path: path.to_string(),
                        found,
                        supported: owned(self.profiles),
                    });
                }
                Some(_) => {}
            }
        }
        if let (Some(path), Some(found)) = (self.version, version.found)
            && !self.versions.contains(&found.as_str())
        {
            return Err(IdentityError::VersionUnsupported {
                path: path.to_string(),
                found,
                supported: owned(self.versions),
            });
        }
        Ok(())
    }

    /// Checks the mandatory root attributes.
    fn check_attributes(&self, root: &BytesStart<'_>) -> Result<(), IdentityError> {
        for (name, supported) in self.attributes {
            let attr = root
                .try_get_attribute(name)
                .map_err(|e| IdentityError::NotXml(e.to_string()))?;
            let Some(attr) = attr else {
                return Err(IdentityError::AttributeMissing {
                    name: (*name).to_string(),
                    supported: owned(supported),
                });
            };
            let value = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| IdentityError::NotXml(e.to_string()))?;
            let value = value.trim();
            if !supported.contains(&value) {
                return Err(IdentityError::AttributeUnsupported {
                    name: (*name).to_string(),
                    found: value.to_string(),
                    supported: owned(supported),
                });
            }
        }
        Ok(())
    }

    /// Reads the text of the first occurrence of each wanted path, from the
    /// document's header (see the module docs for where scanning stops).
    fn scan_header(
        &self,
        reader: &mut NsReader<&[u8]>,
        buf: &mut Vec<u8>,
        wanted: &mut [Wanted],
    ) -> Result<(), IdentityError> {
        let not_xml = |e: &dyn fmt::Display| IdentityError::NotXml(e.to_string());
        // Element local names from the root's child down to the current one.
        let mut stack: Vec<String> = Vec::new();
        // The wanted entry whose element is open, and its own text so far
        // (text of elements nested in it is not part of the value).
        let mut capturing: Option<(usize, String)> = None;
        // Scanning stops at the first top-level element after the profile
        // that lies on no identity path; with no profile, after the first.
        let mut profile_seen = wanted[0].path.is_empty();
        let local = |e: &BytesStart<'_>| e.local_name().as_ref().to_owned();
        // Whether `name`, opened at `stack`, lies on some identity path.
        let on_path = |stack: &[String], name: &str, wanted: &[Wanted]| {
            wanted.iter().any(|w| {
                w.path.len() > stack.len()
                    && w.path.iter().zip(stack).all(|(a, b)| a == b)
                    && w.path[stack.len()] == name
            })
        };
        // The not-yet-found wanted entry whose path is exactly `stack`.
        let at = |stack: &[String], wanted: &[Wanted]| {
            wanted.iter().position(|w| {
                w.found.is_none() && !w.path.is_empty() && w.path.iter().eq(stack.iter())
            })
        };
        // The open capture, when the current element is the captured one.
        fn own_text<'c>(
            capturing: &'c mut Option<(usize, String)>,
            stack: &[String],
            wanted: &[Wanted],
        ) -> Option<&'c mut String> {
            match capturing {
                Some((i, text)) if wanted[*i].path.len() == stack.len() => Some(text),
                _ => None,
            }
        }
        let done = |wanted: &[Wanted]| {
            wanted
                .iter()
                .all(|w| w.path.is_empty() || w.found.is_some())
        };
        loop {
            buf.clear();
            match reader.read_event_into(buf).map_err(|e| not_xml(&e))? {
                Event::Start(e) => {
                    let name = local(&e);
                    if stack.is_empty() && profile_seen && !on_path(&stack, &name, wanted) {
                        return Ok(()); // past the header
                    }
                    stack.push(name);
                    if let Some(i) = at(&stack, wanted) {
                        capturing = Some((i, String::new()));
                    }
                }
                Event::Empty(e) => {
                    let name = local(&e);
                    if stack.is_empty() && profile_seen && !on_path(&stack, &name, wanted) {
                        return Ok(()); // past the header
                    }
                    stack.push(name);
                    if let Some(i) = at(&stack, wanted) {
                        wanted[i].found = Some(String::new());
                        profile_seen |= i == 0;
                    }
                    stack.pop();
                    if done(wanted) {
                        return Ok(());
                    }
                }
                Event::Text(t) => {
                    if let Some(text) = own_text(&mut capturing, &stack, wanted) {
                        text.push_str(&t.xml10_content());
                    }
                }
                Event::CData(t) => {
                    if let Some(text) = own_text(&mut capturing, &stack, wanted) {
                        text.push_str(&t.xml10_content());
                    }
                }
                Event::GeneralRef(r) => {
                    if let Some(text) = own_text(&mut capturing, &stack, wanted) {
                        if let Some(c) = r.resolve_char_ref().map_err(|e| not_xml(&e))? {
                            text.push(c);
                        } else {
                            let name = r.xml10_content();
                            let resolved = resolve_predefined_entity(&name).ok_or_else(|| {
                                IdentityError::NotXml(format!("unknown entity `&{name};`"))
                            })?;
                            text.push_str(resolved);
                        }
                    }
                }
                Event::End(_) => {
                    if stack.is_empty() {
                        return Ok(()); // the root closed
                    }
                    if let Some((i, text)) =
                        capturing.take_if(|(i, _)| wanted[*i].path.len() == stack.len())
                    {
                        wanted[i].found = Some(text.trim().to_string());
                        profile_seen |= i == 0;
                    }
                    stack.pop();
                    if done(wanted) {
                        return Ok(());
                    }
                }
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: Identity = Identity {
        namespace: Some("urn:doc"),
        root: "Doc",
        profile: Some("Context.Profile"),
        profiles: &["urn:a&b"],
        version: Some("Version"),
        versions: &["1.0"],
        attributes: &[("kind", &["K1"])],
    };

    fn doc(body: &str) -> String {
        format!(r#"<d:Doc xmlns:d="urn:doc" kind="K1">{body}</d:Doc>"#)
    }

    const PROFILE: &str = "<Context><Profile> urn:a&amp;b </Profile></Context>";

    #[test]
    fn test_check_accepts_exact_identity() {
        assert_eq!(ID.check(doc(PROFILE).as_bytes()), Ok(()));
        // Version present and supported; CDATA and character references
        // spell the same identifier.
        let body =
            "<Version>1.0</Version><Context><Profile><![CDATA[urn:a]]>&#38;b</Profile></Context>";
        assert_eq!(ID.check(doc(body).as_bytes()), Ok(()));
    }

    #[test]
    fn test_check_root_namespace_name_and_unbound_prefix() {
        let wrong_ns = format!(r#"<d:Doc xmlns:d="urn:other" kind="K1">{PROFILE}</d:Doc>"#);
        assert!(matches!(
            ID.check(wrong_ns.as_bytes()),
            Err(IdentityError::Root { .. })
        ));
        let no_ns = format!(r#"<Doc kind="K1">{PROFILE}</Doc>"#);
        let err = ID.check(no_ns.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("in no namespace"), "{err}");
        let unbound = format!(r#"<x:Doc kind="K1">{PROFILE}</x:Doc>"#);
        assert!(matches!(
            ID.check(unbound.as_bytes()),
            Err(IdentityError::NotXml(_))
        ));
    }

    #[test]
    fn test_check_attributes() {
        let missing = format!(r#"<Doc xmlns="urn:doc">{PROFILE}</Doc>"#);
        assert!(matches!(
            ID.check(missing.as_bytes()),
            Err(IdentityError::AttributeMissing { .. })
        ));
        // A namespaced attribute of the same local name is not the attribute.
        let qualified =
            format!(r#"<Doc xmlns="urn:doc" xmlns:q="urn:q" q:kind="K1">{PROFILE}</Doc>"#);
        assert!(matches!(
            ID.check(qualified.as_bytes()),
            Err(IdentityError::AttributeMissing { .. })
        ));
        let other = format!(r#"<Doc xmlns="urn:doc" kind="K2">{PROFILE}</Doc>"#);
        assert!(matches!(
            ID.check(other.as_bytes()),
            Err(IdentityError::AttributeUnsupported { .. })
        ));
    }

    #[test]
    fn test_check_profile_is_matched_whole_at_its_path() {
        for (body, unsupported) in [
            (
                "<Context><Profile>urn:a&amp;b-extra</Profile></Context>",
                true,
            ),
            ("<Context><Profile>URN:A&amp;B</Profile></Context>", true),
            ("<Context><Profile/></Context>", true),
            // Not at the declared path: absent.
            ("<Profile>urn:a&amp;b</Profile>", false),
            (
                "<Context><X><Profile>urn:a&amp;b</Profile></X></Context>",
                false,
            ),
            ("", false),
        ] {
            let err = ID.check(doc(body).as_bytes()).unwrap_err();
            if unsupported {
                assert!(
                    matches!(err, IdentityError::ProfileUnsupported { .. }),
                    "{body}: {err}"
                );
            } else {
                assert!(
                    matches!(err, IdentityError::ProfileMissing { .. }),
                    "{body}: {err}"
                );
            }
        }
        // The first occurrence is the document's profile.
        let twice = "<Context><Profile>urn:x</Profile><Profile>urn:a&amp;b</Profile></Context>";
        assert!(ID.check(doc(twice).as_bytes()).is_err());
        // Text of an element nested in the profile is not part of it.
        let nested = "<Context><Profile>urn:a<X>&amp;b</X></Profile></Context>";
        assert!(matches!(
            ID.check(doc(nested).as_bytes()),
            Err(IdentityError::ProfileUnsupported { found, .. }) if found == "urn:a"
        ));
    }

    #[test]
    fn test_check_version() {
        let body = format!("<Version>2.0</Version>{PROFILE}");
        assert!(matches!(
            ID.check(doc(&body).as_bytes()),
            Err(IdentityError::VersionUnsupported { .. })
        ));
    }

    #[test]
    fn test_check_reads_only_the_header() {
        // Scanning stops at the first top-level element after the profile
        // that lies on no identity path: content past it is never read, so a
        // malformed tail does not affect the identity.
        let body = format!("{PROFILE}<Content/><Version>9</Version><broken");
        assert_eq!(ID.check(doc(&body).as_bytes()), Ok(()));
    }
}
