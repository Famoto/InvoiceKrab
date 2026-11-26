//! XML primitives for the generated readers and writers.
//!
//! Each generated source model reads a document into a packed
//! [`Values`](crate::record::Values) record and writes one back as XML:
//! element and attribute names are literals in the generated code, and the
//! mechanics they share live here.
//!
//! # Structure
//!
//! - Writing: [`text`] (element text), [`attr`] (a double-quoted attribute
//!   value), [`element`] (a scalar element: `<tag>text</tag>`, or `<tag/>`
//!   when empty).
//! - Reading: [`Reader`], a pull reader over quick-xml that hands the
//!   generated code each child element ([`Elem`]) or text segment
//!   ([`Node`]) of the element it is reading, and each element's attributes
//!   by local name.
//!
//! # Behavior
//!
//! The escapes are byte-for-byte those of quick-xml's serde serializer (its
//! default, `QuoteLevel::Partial`), which wrote the documents before:
//! text escapes `&`, `<`, `>` and `\r`; attributes also `"`, `\n` and `\t`,
//! so a parser's end-of-line and attribute-value normalization cannot change
//! the value. `'` is never escaped. A value with nothing to escape is copied
//! in one `push_str`.
//!
//! Reading follows quick-xml's serde deserializer, which read the documents
//! before, so a document reads the same: names match by local name (any
//! prefix); text is decoded, its references resolved and its line ends
//! normalized; a scalar element's value is its whole content, untrimmed (an
//! empty element is `""`, a child element in it is an error); an element's
//! own text (`$text`) is its first text segment that is not blank, and a
//! second one is an error; text and CDATA separated only by comments and
//! processing instructions are one segment; an attribute value is
//! normalized. An element with `xsi:nil="true"` reads as absent. A repeated
//! element must come in one run (blank text and comments may sit between
//! its items); a single element that repeats, or a run that is
//! interrupted, is the deserializer's `duplicate field` error. Unknown
//! elements are skipped whole, unknown attributes ignored.
//!
//! # Testing
//!
//! Unit tests cover every escaped character in both positions and the
//! reader's text, attribute and nil rules; the interfaces crate reads and
//! writes every sample through the generated code.

use std::borrow::Cow;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{NamespaceResolver, QName};
use quick_xml::{DeError, XmlVersion};

/// Appends `value` as element text.
pub fn text(out: &mut String, value: &str) {
    escape(out, value, |b| matches!(b, b'&' | b'<' | b'>' | b'\r'));
}

/// Appends `value` as the content of a double-quoted attribute.
pub fn attr(out: &mut String, value: &str) {
    escape(out, value, |b| {
        matches!(b, b'&' | b'<' | b'>' | b'"' | b'\r' | b'\n' | b'\t')
    });
}

/// Appends the scalar element `tag` holding `value`: `<tag/>` when `value`
/// is empty.
pub fn element(out: &mut String, tag: &str, value: &str) {
    out.push('<');
    out.push_str(tag);
    if value.is_empty() {
        out.push_str("/>");
    } else {
        out.push('>');
        text(out, value);
        out.push_str("</");
        out.push_str(tag);
        out.push('>');
    }
}

/// A pull reader over one document for the generated readers.
pub struct Reader<'a> {
    inner: quick_xml::Reader<&'a [u8]>,
    /// The namespace declarations in scope, for `xsi:nil`. Only elements
    /// that declare a namespace open a scope (most declare none), at the
    /// depths in `scopes`.
    ns: NamespaceResolver,
    scopes: Vec<usize>,
    /// The number of elements open.
    depth: usize,
    version: XmlVersion,
    /// The event that ended a text segment [`Reader::next`] returned.
    pending: Option<Event<'a>>,
}

/// A start tag the generated reader is looking at: an element to read
/// ([`Reader::text`], [`Reader::next`]) or skip ([`Reader::skip`]).
pub struct Elem<'a> {
    start: BytesStart<'a>,
    empty: bool,
    nil: bool,
    version: XmlVersion,
}

/// What [`Reader::next`] found in the element being read.
pub enum Node<'a> {
    /// A child element.
    Child(Elem<'a>),
    /// A text segment that is not blank, untrimmed.
    Text(Cow<'a, str>),
    /// The element's end.
    End,
}

/// The deserializer's error for an element the document never closes.
fn missed_end(start: &BytesStart<'_>) -> DeError {
    let name = start.name().into_inner().to_string();
    quick_xml::Error::from(quick_xml::errors::IllFormedError::MissingEndTag(name)).into()
}

/// The `missing field` error of serde's derive, for a required value the
/// element lacks.
pub fn missing(name: &str) -> DeError {
    DeError::Custom(format!("missing field `{name}`"))
}

/// The `duplicate field` error of quick-xml's deserializer, which the
/// engine reports as an element repeated where the mapping reads one.
pub fn duplicate(name: &str) -> DeError {
    DeError::Custom(format!("duplicate field `{name}`"))
}

impl<'a> Elem<'a> {
    /// The element's local name.
    pub fn local_name(&self) -> &str {
        self.start.local_name().into_inner()
    }

    /// Whether the element carries `xsi:nil="true"` (it reads as absent).
    pub fn is_nil(&self) -> bool {
        self.nil
    }

    /// The element's attributes as `(local name, normalized value)`, the
    /// namespace declarations and `xml:` attributes left out.
    pub fn attributes(&self) -> impl Iterator<Item = Result<(&str, Cow<'_, str>), DeError>> {
        let version = self.version;
        self.start.attributes().filter_map(move |attr| {
            let attr = match attr {
                Ok(attr) => attr,
                Err(e) => return Some(Err(e.into())),
            };
            let key: QName<'_> = attr.key;
            if key.as_namespace_binding().is_some()
                || key.prefix().is_some_and(|p| p.into_inner() == "xml")
            {
                return None;
            }
            let local = key.local_name().into_inner();
            Some(
                attr.normalized_value(version)
                    .map(|value| (local, value))
                    .map_err(DeError::from),
            )
        })
    }
}

impl<'a> Reader<'a> {
    /// A reader at the start of `document`.
    pub fn new(document: &'a str) -> Self {
        Reader {
            inner: quick_xml::Reader::from_str(document),
            ns: NamespaceResolver::default(),
            scopes: Vec::new(),
            depth: 0,
            version: XmlVersion::Implicit1_0,
            pending: None,
        }
    }

    /// The next event; the XML declaration and the document type are
    /// consumed on the way.
    fn event(&mut self) -> Result<Event<'a>, DeError> {
        if let Some(event) = self.pending.take() {
            return Ok(event);
        }
        loop {
            match self.inner.read_event()? {
                Event::Decl(decl) => self.version = decl.xml_version()?,
                Event::DocType(_) => {}
                event => return Ok(event),
            }
        }
    }

    /// A start tag as an [`Elem`], read while its namespace scope is open.
    fn elem(&mut self, start: BytesStart<'a>, empty: bool) -> Result<Elem<'a>, DeError> {
        self.open(&start)?;
        // Most elements carry no attribute named `nil`: skip resolving them.
        let raw = start.attributes_raw();
        let nil = !raw.is_empty() && raw.contains("nil") && start.attributes().has_nil(&self.ns);
        if empty {
            self.close();
        }
        Ok(Elem {
            start,
            empty,
            nil,
            version: self.version,
        })
    }

    /// Opens the element `start`, with a namespace scope if it declares
    /// one; a malformed declaration is an error, as for quick-xml's
    /// `NsReader`.
    fn open(&mut self, start: &BytesStart<'_>) -> Result<(), DeError> {
        self.depth += 1;
        let raw = start.attributes_raw();
        if !raw.is_empty() && raw.contains("xmlns") {
            self.ns.push(start)?;
            self.scopes.push(self.depth);
        }
        Ok(())
    }

    /// Closes the innermost open element, and its namespace scope.
    fn close(&mut self) {
        if self.scopes.last() == Some(&self.depth) {
            self.scopes.pop();
            self.ns.pop();
        }
        self.depth -= 1;
    }

    /// The root element.
    ///
    /// # Errors
    ///
    /// Malformed XML, or a document without an element.
    pub fn root(&mut self) -> Result<Elem<'a>, DeError> {
        loop {
            match self.event()? {
                Event::Start(start) => return self.elem(start, false),
                Event::Empty(start) => return self.elem(start, true),
                Event::Eof => return Err(DeError::UnexpectedEof),
                _ => {}
            }
        }
    }

    /// Appends a text-like event (text, CDATA, a reference; a comment or a
    /// processing instruction adds nothing) to `text`; `false` for any other
    /// event.
    fn append_text(&self, event: &Event<'a>, text: &mut Cow<'a, str>) -> Result<bool, DeError> {
        let piece: Cow<'a, str> = match event {
            Event::Text(t) => t.xml_content(self.version),
            Event::CData(t) => t.xml_content(self.version),
            Event::GeneralRef(r) => match r.resolve_char_ref()? {
                Some(ch) => Cow::Owned(ch.to_string()),
                None => match quick_xml::escape::resolve_predefined_entity(r) {
                    Some(value) => Cow::Borrowed(value),
                    None => {
                        let name: &str = r;
                        return Err(quick_xml::escape::EscapeError::UnrecognizedEntity(
                            0..name.len(),
                            name.to_string(),
                        )
                        .into());
                    }
                },
            },
            Event::Comment(_) | Event::PI(_) => return Ok(true),
            _ => return Ok(false),
        };
        if text.is_empty() {
            *text = piece;
        } else {
            text.to_mut().push_str(&piece);
        }
        Ok(true)
    }

    /// The next child element or text segment of `parent`, or its end.
    /// Blank text, comments and processing instructions are passed over.
    ///
    /// # Errors
    ///
    /// Malformed XML, or an unknown entity.
    pub fn next(&mut self, parent: &Elem<'a>) -> Result<Node<'a>, DeError> {
        if parent.empty {
            return Ok(Node::End);
        }
        let mut text: Cow<'a, str> = Cow::Borrowed("");
        loop {
            let event = self.event()?;
            if self.append_text(&event, &mut text)? {
                continue;
            }
            if !text
                .bytes()
                .all(|b| matches!(b, b' ' | b'\r' | b'\n' | b'\t'))
            {
                // The segment ends at `event`, returned by the next call.
                self.pending = Some(event);
                return Ok(Node::Text(text));
            }
            text = Cow::Borrowed("");
            match event {
                Event::Start(start) => return Ok(Node::Child(self.elem(start, false)?)),
                Event::Empty(start) => return Ok(Node::Child(self.elem(start, true)?)),
                Event::End(_) => {
                    self.close();
                    return Ok(Node::End);
                }
                Event::Eof => return Err(missed_end(&parent.start)),
                _ => {}
            }
        }
    }

    /// The whole content of the scalar element `elem`: `""` when empty.
    ///
    /// # Errors
    ///
    /// Malformed XML, an unknown entity, or a child element
    /// ([`DeError::MixedContent`]).
    pub fn text(&mut self, elem: Elem<'a>) -> Result<Cow<'a, str>, DeError> {
        if elem.empty {
            return Ok(Cow::Borrowed(""));
        }
        let mut text: Cow<'a, str> = Cow::Borrowed("");
        loop {
            let event = self.event()?;
            if self.append_text(&event, &mut text)? {
                continue;
            }
            match event {
                Event::End(_) => {
                    self.close();
                    return Ok(text);
                }
                Event::Start(child) | Event::Empty(child) => {
                    self.open(&child)?;
                    let name = child.name().into_inner().to_string();
                    return Err(DeError::MixedContent(name));
                }
                Event::Eof => return Err(missed_end(&elem.start)),
                _ => {}
            }
        }
    }

    /// Skips `elem` and everything in it.
    ///
    /// # Errors
    ///
    /// Malformed XML.
    pub fn skip(&mut self, elem: Elem<'a>) -> Result<(), DeError> {
        if !elem.empty {
            self.inner.read_to_end(elem.start.name())?;
            self.close();
        }
        Ok(())
    }
}

/// Appends `value`, replacing each byte `special` picks by its reference.
/// The picked bytes are all ASCII, so every slice boundary is a char boundary.
fn escape(out: &mut String, value: &str, special: impl Fn(u8) -> bool) {
    let mut start = 0;
    for (i, &b) in value.as_bytes().iter().enumerate() {
        if !special(b) {
            continue;
        }
        out.push_str(&value[start..i]);
        out.push_str(match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\r' => "&#13;",
            b'\n' => "&#10;",
            _ => "&#9;",
        });
        start = i + 1;
    }
    out.push_str(&value[start..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "a&b<c>d\"e'f\rg\nh\ti ü";

    /// Reads `doc`'s root children as `(name, text)`, a nested element's
    /// own text segments joined by `|`.
    fn children(doc: &str) -> Result<Vec<(String, String)>, DeError> {
        let mut r = Reader::new(doc);
        let root = r.root()?;
        let mut out = Vec::new();
        loop {
            match r.next(&root)? {
                Node::Child(c) if c.local_name() == "Mixed" => {
                    let name = c.local_name().to_string();
                    let mut parts = Vec::new();
                    loop {
                        match r.next(&c)? {
                            Node::Text(t) => parts.push(t.into_owned()),
                            Node::Child(inner) => r.skip(inner)?,
                            Node::End => break,
                        }
                    }
                    out.push((name, parts.join("|")));
                }
                Node::Child(c) if c.is_nil() => {
                    out.push((c.local_name().to_string(), "<nil>".into()));
                    r.skip(c)?;
                }
                Node::Child(c) => {
                    let name = c.local_name().to_string();
                    out.push((name, r.text(c)?.into_owned()));
                }
                Node::Text(t) => out.push(("$text".into(), t.into_owned())),
                Node::End => return Ok(out),
            }
        }
    }

    #[test]
    fn test_reader_text_is_whole_untrimmed_and_decoded() {
        let doc = "<?xml version=\"1.0\"?><!DOCTYPE r><r xmlns:c=\"urn:c\">\n  \
                   <c:ID> a &amp; b </c:ID><Empty/><Open></Open>\
                   <Joined>x<!-- c --><![CDATA[<y>]]>&#65;\r\nz</Joined></r>";
        let got = children(doc).unwrap();
        let want = [
            ("ID", " a & b "),
            ("Empty", ""),
            ("Open", ""),
            ("Joined", "x<y>A\nz"),
        ];
        let got: Vec<_> = got.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn test_reader_own_text_segments_skip_blank_ones() {
        let doc = "<r><Mixed>\n <a/> first <b>x</b>\n\t<c/>second</Mixed></r>";
        assert_eq!(
            children(doc).unwrap(),
            [("Mixed".into(), " first |second".into())]
        );
    }

    #[test]
    fn test_reader_attributes_by_local_name_normalized() {
        let doc = "<r xmlns:x=\"urn:x\" xml:lang=\"de\" x:currencyID=\"E\tU&#x52;\" a=''/>";
        let mut r = Reader::new(doc);
        let root = r.root().unwrap();
        let attrs: Vec<_> = root
            .attributes()
            .map(|a| a.map(|(k, v)| (k.to_string(), v.into_owned())))
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            attrs,
            [
                ("currencyID".into(), "E UR".into()),
                ("a".into(), String::new())
            ]
        );
        assert!(matches!(r.next(&root), Ok(Node::End)));
    }

    #[test]
    fn test_reader_nil_and_errors() {
        let doc = "<r xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
                   <A xsi:nil=\"true\"><x/></A><B xsi:nil=\"false\">b</B></r>";
        assert_eq!(
            children(doc).unwrap(),
            [("A".into(), "<nil>".into()), ("B".into(), "b".into())]
        );
        assert!(matches!(
            children("<r><A>x<y/></A></r>"),
            Err(DeError::MixedContent(name)) if name == "y"
        ));
        assert!(children("<r><A>&nope;</A></r>").is_err());
        assert_eq!(
            children("<r><A>x</A>").unwrap_err().to_string(),
            "ill-formed document: start tag not closed: `</r>` not found before end of input"
        );
        assert!(children("<r><A>x").is_err(), "an unclosed scalar");
        assert!(Reader::new("<!-- only -->").root().is_err());
    }

    #[test]
    fn test_reader_nil_follows_namespace_scopes() {
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        let doc = format!(
            "<r><A xmlns:i=\"{xsi}\" i:nil=\"true\"/><B i:nil=\"true\">b</B>\
             <C xmlns:i=\"urn:other\" i:nil=\"true\">c</C>\
             <D xmlns:j=\"{xsi}\"><E j:nil=\"true\"><x/></E></D><F j:nil=\"true\">f</F></r>"
        );
        let mut r = Reader::new(&doc);
        let root = r.root().unwrap();
        let mut seen = Vec::new();
        while let Node::Child(c) = r.next(&root).unwrap() {
            seen.push((c.local_name().to_string(), c.is_nil()));
            if c.local_name() == "D" {
                let Node::Child(e) = r.next(&c).unwrap() else {
                    panic!("E")
                };
                seen.push((e.local_name().to_string(), e.is_nil()));
                r.skip(e).unwrap();
                assert!(matches!(r.next(&c).unwrap(), Node::End));
            } else {
                r.skip(c).unwrap();
            }
        }
        let nil: Vec<_> = seen
            .iter()
            .map(|(n, nil)| format!("{n}{}", if *nil { "!" } else { "" }))
            .collect();
        assert_eq!(
            nil,
            ["A!", "B", "C", "D", "E!", "F"],
            "a scope ends with its element"
        );
        assert!(
            children("<r><A xmlns:xml=\"urn:x\">a</A></r>").is_err(),
            "a malformed declaration is refused"
        );
    }

    #[test]
    fn test_text_escapes_markup_and_carriage_return_only() {
        let mut out = String::new();
        text(&mut out, RAW);
        assert_eq!(out, "a&amp;b&lt;c&gt;d\"e'f&#13;g\nh\ti ü");
    }

    #[test]
    fn test_attr_also_escapes_quote_newline_and_tab() {
        let mut out = String::new();
        attr(&mut out, RAW);
        assert_eq!(out, "a&amp;b&lt;c&gt;d&quot;e'f&#13;g&#10;h&#9;i ü");
    }

    #[test]
    fn test_element_is_self_closed_when_empty() {
        let mut out = String::new();
        element(&mut out, "cbc:ID", "");
        element(&mut out, "cbc:Note", "1 < 2");
        assert_eq!(out, "<cbc:ID/><cbc:Note>1 &lt; 2</cbc:Note>");
    }

    #[test]
    fn test_plain_value_is_copied_unchanged() {
        let mut out = String::from("x");
        text(&mut out, "INV-42");
        attr(&mut out, "");
        assert_eq!(out, "xINV-42");
    }
}
