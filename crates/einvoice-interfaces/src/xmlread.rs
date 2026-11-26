//! Pull-parser helpers for the generated XML readers.
//!
//! The generated `read_xml` of each source struct drives a [`XmlIn`]: it
//! matches child elements and attributes by local name and reads text through
//! [`XmlIn::text`]. The semantics follow quick-xml's serde deserializer, the
//! reference the generated readers are compared against: local names, text
//! runs (text, CDATA, references) concatenated and trimmed of XML whitespace,
//! comments and processing instructions skipped, unknown elements skipped,
//! a single-valued element read twice is ``duplicate field `X` ``.

use std::borrow::Cow;

use compact_str::CompactString;
use quick_xml::DeError;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// The event stream of one document.
pub struct XmlIn<'a> {
    reader: Reader<&'a [u8]>,
    text: String,
    pending: Option<Ev<'a>>,
}

/// One element-level event, text already merged.
pub enum Ev<'a> {
    /// `<tag ...>`.
    Start(BytesStart<'a>),
    /// `<tag .../>`.
    Empty(BytesStart<'a>),
    /// `</tag>`.
    End,
    /// A non-blank text run, at element level (untrimmed, as serde reads it).
    Text(CompactString),
}

fn de(error: impl std::fmt::Display) -> DeError {
    DeError::Custom(error.to_string())
}

/// XML whitespace (`S` in the specification).
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// Appends the replacement of a `&ref;` reference.
fn push_ref(out: &mut String, r: &BytesRef<'_>) -> Result<(), DeError> {
    if let Some(c) = r.resolve_char_ref().map_err(de)? {
        out.push(c);
        return Ok(());
    }
    match quick_xml::escape::resolve_predefined_entity(r) {
        Some(value) => {
            out.push_str(value);
            Ok(())
        }
        None => Err(de(format!("unrecognized entity `{}`", &**r))),
    }
}

impl<'a> XmlIn<'a> {
    /// A reader over `s`.
    pub fn new(s: &'a str) -> Self {
        XmlIn {
            reader: Reader::from_str(s),
            text: String::new(),
            pending: None,
        }
    }

    /// The document element: skips the prolog (declaration, comments,
    /// processing instructions, doctype, whitespace).
    pub fn root(&mut self) -> Result<(BytesStart<'a>, bool), DeError> {
        loop {
            match self.reader.read_event().map_err(de)? {
                Event::Start(e) => return Ok((e, false)),
                Event::Empty(e) => return Ok((e, true)),
                Event::Eof => return Err(DeError::UnexpectedEof),
                _ => {}
            }
        }
    }

    /// The next element-level event inside the current element: blank text
    /// runs, comments and processing instructions are skipped.
    pub fn next(&mut self) -> Result<Ev<'a>, DeError> {
        if let Some(event) = self.pending.take() {
            return Ok(event);
        }
        loop {
            let event = self.reader.read_event().map_err(de)?;
            match event {
                Event::Start(e) => return Ok(Ev::Start(e)),
                Event::Empty(e) => return Ok(Ev::Empty(e)),
                Event::End(_) => return Ok(Ev::End),
                // Indentation between elements: blank, and alone in its run
                // unless CDATA or a reference follows (then copied below).
                Event::Text(t) if t.trim_matches(is_ws).is_empty() => {
                    let next = self.reader.read_event().map_err(de)?;
                    match next {
                        Event::Start(e) => return Ok(Ev::Start(e)),
                        Event::Empty(e) => return Ok(Ev::Empty(e)),
                        Event::End(_) => return Ok(Ev::End),
                        Event::CData(_) | Event::GeneralRef(_) | Event::Text(_) => {
                            self.text.clear();
                            self.text.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                            if let Some(ev) = self.run_text(next)? {
                                return Ok(ev);
                            }
                        }
                        Event::Eof => return Err(DeError::UnexpectedEof),
                        _ => {}
                    }
                }
                Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                    self.text.clear();
                    if let Some(ev) = self.run_text(event)? {
                        return Ok(ev);
                    }
                }
                Event::Eof => return Err(DeError::UnexpectedEof),
                _ => {}
            }
        }
    }

    /// Finishes a text run that starts with `event` (after what `self.text`
    /// already holds): the run as `Ev::Text` (the event that ended it
    /// pending), or the ending event itself when the run is blank.
    fn run_text(&mut self, event: Event<'a>) -> Result<Option<Ev<'a>>, DeError> {
        let next = self.run(event)?;
        if self.text.trim_matches(is_ws).is_empty() {
            return Ok(Some(next));
        }
        self.pending = Some(next);
        Ok(Some(Ev::Text(CompactString::from(self.text.as_str()))))
    }

    /// Appends the text run starting with `first` to `self.text`; returns the
    /// element-level event that ended it.
    fn run(&mut self, first: Event<'a>) -> Result<Ev<'a>, DeError> {
        let mut event = first;
        loop {
            match event {
                Event::Text(t) => self.text.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Event::CData(t) => self.text.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
                Event::GeneralRef(r) => push_ref(&mut self.text, &r)?,
                Event::Start(e) => return Ok(Ev::Start(e)),
                Event::Empty(e) => return Ok(Ev::Empty(e)),
                Event::End(_) => return Ok(Ev::End),
                Event::Eof => return Err(DeError::UnexpectedEof),
                _ => {}
            }
            event = self.reader.read_event().map_err(de)?;
        }
    }

    /// The text content of the element just started (`empty`: it was
    /// `<tag/>`), untrimmed; a nested element is an error (mixed content).
    pub fn text(&mut self, empty: bool) -> Result<CompactString, DeError> {
        if empty {
            return Ok(CompactString::default());
        }
        // The common case: one text event, then the end tag; no copy but
        // the value's own.
        let first = self.reader.read_event().map_err(de)?;
        let mut out = String::new();
        match first {
            Event::End(_) => return Ok(CompactString::default()),
            Event::Text(t) => {
                let content = t.xml_content(XmlVersion::Implicit1_0);
                let next = self.reader.read_event().map_err(de)?;
                if let Event::End(_) = next {
                    return Ok(CompactString::from(content.as_ref()));
                }
                out.push_str(&content);
                self.text_event(next, &mut out)?;
            }
            other => self.text_event(other, &mut out)?,
        }
        loop {
            let event = self.reader.read_event().map_err(de)?;
            if let Event::End(_) = event {
                return Ok(CompactString::from(out));
            }
            self.text_event(event, &mut out)?;
        }
    }

    /// One event inside a scalar element: text is appended, comments and
    /// processing instructions skipped, an element is mixed content (serde
    /// refuses it).
    fn text_event(&mut self, event: Event<'a>, out: &mut String) -> Result<(), DeError> {
        match event {
            Event::Text(t) => out.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
            Event::CData(t) => out.push_str(&t.xml_content(XmlVersion::Implicit1_0)),
            Event::GeneralRef(r) => push_ref(out, &r)?,
            Event::Start(e) | Event::Empty(e) => {
                return Err(DeError::Custom(format!(
                    "mixed content in `{}`",
                    e.name().into_inner()
                )));
            }
            Event::End(_) => unreachable!("handled by the caller"),
            Event::Eof => return Err(DeError::UnexpectedEof),
            _ => {}
        }
        Ok(())
    }

    /// Skips the element just started (`empty`: it was `<tag/>`).
    pub fn skip(&mut self, start: &BytesStart<'_>, empty: bool) -> Result<(), DeError> {
        if !empty {
            self.reader.read_to_end(start.name()).map_err(de)?;
        }
        Ok(())
    }
}

/// The name an element is matched by: its local name.
pub fn local<'b>(start: &'b BytesStart<'_>) -> &'b [u8] {
    let name = start.name().into_inner().as_bytes();
    match name.iter().position(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// The name an attribute is matched by: its local name (namespace
/// declarations and `xml:` attributes keep their prefix, and match nothing).
pub fn attr_key<'b>(attr: &'b Attribute<'_>) -> &'b [u8] {
    let name = attr.key.into_inner().as_bytes();
    if name.starts_with(b"xmlns") || name.starts_with(b"xml:") {
        return b"";
    }
    match name.iter().position(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// An attribute's value, normalized and unescaped.
pub fn attr_value(attr: &Attribute<'_>) -> Result<CompactString, DeError> {
    let value: Cow<'_, str> = attr.normalized_value(XmlVersion::Implicit1_0).map_err(de)?;
    Ok(CompactString::from(value.as_ref()))
}

/// Whether the element carries `xsi:nil="true"` (serde reads it as `None`).
// ponytail: matches any prefix with local name `nil`; serde resolves the prefix
// to the XSI namespace, which would need namespace tracking.
pub fn is_nil(start: &BytesStart<'_>) -> bool {
    if !start.attributes_raw().contains(":nil") {
        return false;
    }
    start.attributes().flatten().any(|a| {
        let k = a.key.into_inner().as_bytes();
        k.len() > 4 && k.ends_with(b":nil") && a.as_bool().unwrap_or_default()
    })
}

/// The error serde reports for a field read twice.
pub fn duplicate(name: &str) -> DeError {
    DeError::Custom(format!("duplicate field `{name}`"))
}

/// Compares the generated reader's result with serde's (tests only): equal
/// structs, or both refused; a duplicate-field refusal must also keep serde's
/// text, which the engine's refusal message builds on.
#[cfg(test)]
pub fn same_as_serde<T: PartialEq + std::fmt::Debug>(
    ours: Result<T, DeError>,
    serde: Result<T, DeError>,
) -> Result<(), String> {
    match (ours, serde) {
        (Ok(a), Ok(b)) if a == b => Ok(()),
        (Ok(a), Ok(b)) => Err(format!("read differently:\n{a:#?}\nserde:\n{b:#?}")),
        (Err(a), Err(b)) => {
            let (a, b) = (a.to_string(), b.to_string());
            if b.starts_with("duplicate field") && a != b {
                Err(format!("refused as `{a}`, serde: `{b}`"))
            } else {
                Ok(())
            }
        }
        (Ok(_), Err(b)) => Err(format!("read, serde refused: {b}")),
        (Err(a), Ok(_)) => Err(format!("refused: {a}, serde read it")),
    }
}
