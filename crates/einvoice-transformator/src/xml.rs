//! XML writing primitives for the generated writers.
//!
//! Each generated source struct has a `write_xml` method that writes its
//! element straight into the output `String`: tag names are literals in the
//! generated code, and values go through these escapes.
//!
//! # Structure
//!
//! - [`text`] — element text.
//! - [`attr`] — a double-quoted attribute value.
//! - [`element`] — a scalar element: `<tag>text</tag>`, or `<tag/>` when empty.
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
//! # Testing
//!
//! Unit tests cover every escaped character in both positions; the
//! interfaces crate checks every generated writer against quick-xml on every
//! sample and target.

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
