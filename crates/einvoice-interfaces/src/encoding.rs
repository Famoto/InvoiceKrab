//! Source character encodings: any supported encoding in, UTF-8 out.
//!
//! The generated readers and the identity check work on UTF-8. XML 1.0 lets a
//! document declare another encoding, and real invoices do: FatturaPA and
//! older ERP exports are often ISO-8859-1 / windows-1252, some tools write
//! UTF-16. [`to_utf8`] runs once at the crate boundary and hands the rest of
//! the engine UTF-8 bytes.
//!
//! # Behavior
//!
//! | Input                                             | Result                       |
//! |---------------------------------------------------|------------------------------|
//! | UTF-8 (BOM, `encoding="UTF-8"`, or no declaration) | borrowed unchanged (no copy) |
//! | `ISO-8859-1`, `latin1`, `windows-1252`, `US-ASCII` | transcoded as windows-1252   |
//! | UTF-16 LE/BE (BOM, or `<?` sniffed per XML App. F) | transcoded                   |
//! | any other declared encoding                       | [`EncodingError::Unsupported`] |
//!
//! ISO-8859-1 and US-ASCII are decoded as windows-1252, as the WHATWG
//! Encoding Standard does: documents labelled Latin-1 routinely carry
//! windows-1252 characters such as `€` (0x80), and windows-1252 is a superset
//! of both on every byte a conforming document can hold.
//!
//! A transcoded document's declaration is rewritten to `encoding="UTF-8"`, so
//! it stays truthful about the bytes handed on. Transcoding copies the
//! document once (at most 3x its size for windows-1252, 1.5x for UTF-16).
//!
//! # Testing
//!
//! Unit tests cover each row of the table, the declaration rewrite, and
//! malformed UTF-16.

use std::borrow::Cow;

/// A document whose character encoding cannot be turned into UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodingError {
    /// The XML declaration names an encoding this engine does not decode.
    #[error(
        "unsupported encoding {0:?} (supported: UTF-8, UTF-16, ISO-8859-1, windows-1252, US-ASCII)"
    )]
    Unsupported(String),
    /// The bytes are not valid in the encoding the document uses.
    #[error("invalid {0} byte sequence")]
    Malformed(&'static str),
}

/// The byte order of a UTF-16 document.
#[derive(Debug, Clone, Copy)]
enum Utf16 {
    Le,
    Be,
}

/// Returns `bytes` as UTF-8: borrowed when already UTF-8, otherwise
/// transcoded with the XML declaration's encoding rewritten to `UTF-8`.
///
/// UTF-8 validity itself is not checked here; the XML parser reports it.
///
/// # Errors
///
/// [`EncodingError::Unsupported`] for a declared encoding outside the table
/// in the module docs; [`EncodingError::Malformed`] for broken UTF-16.
pub fn to_utf8(bytes: &[u8]) -> Result<Cow<'_, [u8]>, EncodingError> {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        return Ok(Cow::Borrowed(bytes));
    }
    if let Some((order, skip)) = sniff_utf16(bytes) {
        let text = decode_utf16(&bytes[skip..], order)?;
        return Ok(Cow::Owned(declare_utf8(text).into_bytes()));
    }
    let Some(label) = declared_encoding(bytes) else {
        return Ok(Cow::Borrowed(bytes));
    };
    match label.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" => Ok(Cow::Borrowed(bytes)),
        "iso-8859-1" | "iso8859-1" | "iso_8859-1" | "latin1" | "l1" | "windows-1252" | "cp1252"
        | "us-ascii" | "ascii" => Ok(Cow::Owned(
            declare_utf8(bytes.iter().map(|&b| windows_1252(b)).collect()).into_bytes(),
        )),
        // UTF-16 declared on bytes that are not UTF-16 encoded is a lie the
        // parser would choke on anyway; say so plainly.
        _ => Err(EncodingError::Unsupported(label)),
    }
}

/// Detects UTF-16 by its byte-order mark, or by `<?` encoded in UTF-16
/// without one (XML 1.0 Appendix F); returns the byte order and the number
/// of leading bytes to skip.
fn sniff_utf16(bytes: &[u8]) -> Option<(Utf16, usize)> {
    match bytes {
        [0xFF, 0xFE, ..] => Some((Utf16::Le, 2)),
        [0xFE, 0xFF, ..] => Some((Utf16::Be, 2)),
        [b'<', 0, b'?', 0, ..] => Some((Utf16::Le, 0)),
        [0, b'<', 0, b'?', ..] => Some((Utf16::Be, 0)),
        _ => None,
    }
}

/// Decodes UTF-16 code units in `order`.
fn decode_utf16(bytes: &[u8], order: Utf16) -> Result<String, EncodingError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(EncodingError::Malformed("UTF-16"));
    }
    let units = bytes.chunks_exact(2).map(|pair| match order {
        Utf16::Le => u16::from_le_bytes([pair[0], pair[1]]),
        Utf16::Be => u16::from_be_bytes([pair[0], pair[1]]),
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| EncodingError::Malformed("UTF-16"))
}

/// The `encoding` pseudo-attribute of an ASCII-compatible XML declaration,
/// `None` without a declaration or without the attribute (both mean UTF-8).
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    let range = encoding_value_range(bytes)?;
    Some(String::from_utf8_lossy(&bytes[range]).into_owned())
}

/// The byte range of the `encoding` value inside the leading XML declaration.
fn encoding_value_range(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    if !bytes.starts_with(b"<?xml") {
        return None;
    }
    let end = bytes.windows(2).position(|w| w == b"?>")?;
    let decl = &bytes[..end];
    let at = decl.windows(8).position(|w| w == b"encoding")? + 8;
    let mut i = at;
    let skip_ws = |mut i: usize| {
        while decl.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    i = skip_ws(i);
    if decl.get(i) != Some(&b'=') {
        return None;
    }
    i = skip_ws(i + 1);
    let quote = *decl.get(i).filter(|q| matches!(q, b'"' | b'\''))?;
    let start = i + 1;
    let len = decl[start..].iter().position(|&b| b == quote)?;
    Some(start..start + len)
}

/// Rewrites the declaration of an already-decoded document to say UTF-8.
fn declare_utf8(mut text: String) -> String {
    if let Some(range) = encoding_value_range(text.as_bytes()) {
        text.replace_range(range, "UTF-8");
    }
    text
}

/// Decodes one windows-1252 byte (WHATWG mapping: the five unassigned bytes
/// map to the C1 control of the same value).
fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utf8_without_declaration_is_borrowed() {
        let doc = b"<a>caf\xC3\xA9</a>";
        assert!(matches!(to_utf8(doc), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn test_declared_utf8_and_bom_are_borrowed() {
        let declared = b"<?xml version=\"1.0\" encoding='utf-8'?><a/>";
        assert!(matches!(to_utf8(declared), Ok(Cow::Borrowed(_))));
        let bom = b"\xEF\xBB\xBF<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a/>";
        assert!(
            matches!(to_utf8(bom), Ok(Cow::Borrowed(_))),
            "a BOM overrides the declaration"
        );
    }

    #[test]
    fn test_latin1_is_transcoded_and_redeclared() {
        let doc = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a>Caff\xE8 \x80</a>";
        let out = to_utf8(doc).expect("supported");
        assert_eq!(
            std::str::from_utf8(&out).expect("UTF-8 out"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><a>Caff\u{E8} \u{20AC}</a>"
        );
    }

    #[test]
    fn test_windows_1252_label_with_spaces_and_single_quotes() {
        let doc = b"<?xml version='1.0' encoding = 'windows-1252' ?><a>\x93q\x94</a>";
        let out = to_utf8(doc).expect("supported");
        assert_eq!(
            std::str::from_utf8(&out).expect("UTF-8 out"),
            "<?xml version='1.0' encoding = 'UTF-8' ?><a>\u{201C}q\u{201D}</a>"
        );
    }

    #[test]
    fn test_utf16_with_bom_both_byte_orders() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><a>\u{E8}</a>";
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><a>\u{E8}</a>";
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        for doc in [le, be] {
            let out = to_utf8(&doc).expect("supported");
            assert_eq!(std::str::from_utf8(&out).expect("UTF-8 out"), expected);
        }
    }

    #[test]
    fn test_utf16_without_bom_is_sniffed() {
        let doc: Vec<u8> = "<?xml version=\"1.0\"?><a/>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let out = to_utf8(&doc).expect("supported");
        assert_eq!(&*out, b"<?xml version=\"1.0\"?><a/>");
    }

    #[test]
    fn test_odd_length_utf16_is_malformed() {
        assert_eq!(
            to_utf8(b"\xFF\xFE<\0a"),
            Err(EncodingError::Malformed("UTF-16"))
        );
    }

    #[test]
    fn test_unknown_encoding_is_unsupported() {
        let doc = b"<?xml version=\"1.0\" encoding=\"EBCDIC-US\"?><a/>";
        assert_eq!(
            to_utf8(doc),
            Err(EncodingError::Unsupported("EBCDIC-US".into()))
        );
    }
}
