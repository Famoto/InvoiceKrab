//! Packed records: the in-memory form of a source document.
//!
//! A generated source model knows every element and attribute a mapping can
//! read or write, so a document needs no tree in memory, only its values.
//! A [`Values`] buffer holds the values of one *record* (the root element, or
//! one item of a repeated element) and of everything nested in it, as a flat
//! byte string of entries:
//!
//! ```text
//! entry = varint(id) varint(len) payload
//! ```
//!
//! The `id` is the field's number in its record's layout, which the code
//! generator assigns; element names, nesting and absent fields cost nothing.
//! The payload of a scalar field is its UTF-8 text. The payload of a
//! repeated element's item is that item's own entries, nested in place, so a
//! whole document is one allocation. Interior elements (an `Item`, a
//! `PostalAddress`) have no entry of their own: their fields carry ids from a
//! contiguous range, so "is the `Item` there" is "is any id of its range
//! there" ([`Rec::any`]). An empty entry under an interior's own id *marks*
//! the element as present without content ([`Values::mark`]).
//!
//! # Structure
//!
//! - [`Values`] — an owned record buffer: append, set, mark, nest.
//! - [`Rec`] — a borrowed view of one record (the root, or a nested item):
//!   lookups by id, item iteration, range tests.
//! - [`Index`] — a [`Rec`] with each id's entries located once, for a
//!   record that is looked up many times (the root, an item being mapped).
//! - [`Draft`] — a record being written field by field: [`Values`] that
//!   keep their ids located as they grow.
//!
//! # Behavior
//!
//! A [`Rec`] lookup scans the record's entries, a nested item skipped by its
//! length; an [`Index`] lookup goes straight to the id's first entry. A
//! value is found under the first entry with its id; repeated scalars and
//! items are the entries sharing an id, in order. Lengths are LEB128 varints; a nested record written by the
//! reader reserves a fixed five-byte, zero-padded varint for its length,
//! which decodes like any other.
//!
//! # Testing
//!
//! Unit tests cover lookup, replacement, marks, nesting and the range tests.

use std::fmt;

/// An owned, packed record (see the [module docs](self)).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Values(Vec<u8>);

/// A borrowed view of one packed record: the root's [`Values`], or one item
/// nested in it.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Rec<'a>(&'a [u8]);

/// Width of the length a nested record reserves while it is being read.
const RESERVED: usize = 5;

fn put_varint(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &[u8], at: &mut usize) -> usize {
    let mut v = 0usize;
    let mut shift = 0;
    loop {
        let byte = bytes[*at];
        *at += 1;
        v |= usize::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// The entries of a record: `(id, entry start..end, payload)`.
#[derive(Clone)]
struct Entries<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u32, (usize, usize), &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.bytes.len() {
            return None;
        }
        let start = self.at;
        let id = get_varint(self.bytes, &mut self.at) as u32;
        let len = get_varint(self.bytes, &mut self.at);
        let payload = &self.bytes[self.at..self.at + len];
        self.at += len;
        Some((id, (start, self.at), payload))
    }
}

/// Payloads of scalar entries are written from `&str` only.
fn text(payload: &[u8]) -> &str {
    std::str::from_utf8(payload).unwrap_or_default()
}

impl<'a> From<&'a [u8]> for Rec<'a> {
    /// A nested record's payload ([`Rec::entries`]) as a record.
    fn from(payload: &'a [u8]) -> Self {
        Rec(payload)
    }
}

impl<'a> Rec<'a> {
    fn raw_entries(self) -> Entries<'a> {
        Entries {
            bytes: self.0,
            at: 0,
        }
    }

    /// Every entry as `(id, payload)`, in order; a nested item's payload is
    /// its record ([`Rec::from`]).
    pub fn entries(self) -> impl Iterator<Item = (u32, &'a [u8])> + Clone + 'a {
        self.raw_entries().map(|(id, _, payload)| (id, payload))
    }

    /// The first value under `id`.
    pub fn get(self, id: u32) -> Option<&'a str> {
        self.raw_entries()
            .find(|(i, _, _)| *i == id)
            .map(|(_, _, payload)| text(payload))
    }

    /// The first value under `id` as the [`Field`] type `F` (a packed
    /// struct's field, read without unpacking the struct).
    pub fn field<F: Field>(self, id: u32) -> Option<F> {
        self.raw_entries()
            .find(|(i, _, _)| *i == id)
            .map(|(_, _, payload)| F::read(payload))
    }

    /// Every value under `id` (a repeated scalar), in order.
    pub fn all(self, id: u32) -> impl Iterator<Item = &'a str> + Clone + 'a {
        self.entries()
            .filter(move |(i, _)| *i == id)
            .map(|(_, payload)| text(payload))
    }

    /// Every item nested under `id` (a repeated element), in order.
    pub fn items(self, id: u32) -> impl Iterator<Item = Rec<'a>> + Clone + 'a {
        self.entries()
            .filter(move |(i, _)| *i == id)
            .map(|(_, payload)| Rec(payload))
    }

    /// Whether any entry has an id in `lo..hi`: the element owning that
    /// range is present (a mark or an empty value counts).
    pub fn any(self, lo: u32, hi: u32) -> bool {
        self.entries().any(|(i, _)| (lo..hi).contains(&i))
    }

    /// Whether any entry with an id in `lo..hi` has content: an empty value
    /// and a mark do not count, as an empty string and an empty element did
    /// not count in a struct's emptiness.
    pub fn any_value(self, lo: u32, hi: u32) -> bool {
        self.entries()
            .any(|(i, payload)| (lo..hi).contains(&i) && !payload.is_empty())
    }

    /// Whether no entry has content.
    pub fn is_empty(self) -> bool {
        self.entries().all(|(_, payload)| payload.is_empty())
    }

    /// The record's size in bytes.
    pub fn len(self) -> usize {
        self.0.len()
    }

    /// A copy of the record.
    pub fn to_values(self) -> Values {
        Values(self.0.to_vec())
    }

    /// The entries with an id in `lo..hi` as a record of their own, ids
    /// shifted down by `lo`: an element laid out inline, taken out as an item.
    pub fn extract(self, lo: u32, hi: u32) -> Values {
        let mut out = Values::default();
        for (id, payload) in self.entries() {
            if (lo..hi).contains(&id) {
                out.push_raw(id - lo, payload);
            }
        }
        out
    }
}

/// A [`Rec`] with the span of each id's entries (its first entry's start to
/// its last entry's end) located in one pass, so a lookup reads no entry of
/// another id outside that span. The generated readers and writers look a
/// record's fields up one by one, which on a [`Rec`] would scan the record
/// once per field.
#[derive(Clone, Debug)]
pub struct Index<'a> {
    rec: Rec<'a>,
    /// Per id: its first entry's start, that entry's payload start and end,
    /// and its last entry's end; all 0 when the id has no entry.
    spans: Vec<[u32; 4]>,
}

impl<'a> Index<'a> {
    /// Indexes `rec`, whose layout has `size` ids (`0..size`).
    pub fn new(rec: Rec<'a>, size: u32) -> Self {
        assert!(u32::try_from(rec.len()).is_ok(), "a record over 4 GiB");
        // An entry ends past 0, so a last end of 0 is "no entry".
        let mut spans = vec![[0u32; 4]; size as usize];
        for (id, (start, end), payload) in rec.raw_entries() {
            debug_assert!(id < size, "id {id} outside a layout of {size}");
            if let Some(span) = spans.get_mut(id as usize) {
                if span[3] == 0 {
                    *span = [start as u32, (end - payload.len()) as u32, end as u32, 0];
                }
                span[3] = end as u32;
            }
        }
        Index { rec, spans }
    }

    /// The indexed record.
    pub fn rec(&self) -> Rec<'a> {
        self.rec
    }

    /// The entries from `id`'s first to its last, as a record.
    fn span(&self, id: u32) -> Rec<'a> {
        match self.spans.get(id as usize) {
            Some(&[start, _, _, end]) => Rec(&self.rec.0[start as usize..end as usize]),
            None => Rec::default(),
        }
    }

    /// See [`Rec::get`].
    pub fn get(&self, id: u32) -> Option<&'a str> {
        match self.spans.get(id as usize) {
            Some(&[_, start, end, last]) if last != 0 => {
                Some(text(&self.rec.0[start as usize..end as usize]))
            }
            _ => None,
        }
    }

    /// See [`Rec::all`].
    pub fn all(&self, id: u32) -> impl Iterator<Item = &'a str> + Clone + 'a {
        self.span(id).all(id)
    }

    /// See [`Rec::items`].
    pub fn items(&self, id: u32) -> impl Iterator<Item = Rec<'a>> + Clone + 'a {
        self.span(id).items(id)
    }

    /// See [`Rec::any`].
    pub fn any(&self, lo: u32, hi: u32) -> bool {
        let hi = (hi as usize).min(self.spans.len());
        self.spans
            .get(lo as usize..hi)
            .is_some_and(|spans| spans.iter().any(|span| span[3] != 0))
    }
}

/// A record being built by a generated writer: [`Values`] with the span of
/// each id's entries kept as entries are added, so setting a field replaces
/// its entry without scanning the record.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    values: Values,
    spans: Vec<(usize, usize)>,
}

impl Draft {
    /// An empty record whose layout has `size` ids (`0..size`).
    pub fn new(size: u32) -> Self {
        Draft {
            values: Values::default(),
            spans: vec![(0, 0); size as usize],
        }
    }

    /// Notes the entry `start..` just appended under `id`.
    fn appended(&mut self, id: u32, start: usize) {
        let end = self.values.len();
        debug_assert!(
            (id as usize) < self.spans.len(),
            "id {id} outside the layout"
        );
        if let Some(span) = self.spans.get_mut(id as usize) {
            if span.1 == 0 {
                span.0 = start;
            }
            span.1 = end;
        }
    }

    /// The record as a view.
    pub fn rec(&self) -> Rec<'_> {
        self.values.rec()
    }

    /// The finished record.
    pub fn into_values(self) -> Values {
        self.values
    }

    /// The entries from `id`'s first to its last, as a record.
    fn span(&self, id: u32) -> Rec<'_> {
        match self.spans.get(id as usize) {
            Some(&(start, end)) => Rec(&self.values.0[start..end]),
            None => Rec::default(),
        }
    }

    /// See [`Rec::get`].
    pub fn get(&self, id: u32) -> Option<&str> {
        self.span(id).get(id)
    }

    /// See [`Rec::items`].
    pub fn items(&self, id: u32) -> impl Iterator<Item = Rec<'_>> + Clone + '_ {
        self.span(id).items(id)
    }

    /// See [`Values::push`].
    pub fn push(&mut self, id: u32, value: &str) {
        let start = self.values.len();
        self.values.push(id, value);
        self.appended(id, start);
    }

    /// See [`Values::push_record`].
    pub fn push_record(&mut self, id: u32, item: Rec<'_>) {
        let start = self.values.len();
        self.values.push_record(id, item);
        self.appended(id, start);
    }

    /// See [`Values::mark`].
    pub fn mark(&mut self, id: u32) {
        if self.span(id).0.is_empty() {
            self.push(id, "");
        }
    }

    /// See [`Values::set`].
    pub fn set(&mut self, id: u32, value: &str) {
        let start = match self.spans.get(id as usize) {
            Some(&(start, end)) if end != 0 => start,
            _ => return self.push(id, value),
        };
        let before = self.values.len();
        self.values.replace_at(start, id, value);
        // Every offset past the replaced entry's start moves with its end.
        let grown = self.values.len() as isize - before as isize;
        for span in &mut self.spans {
            for offset in [&mut span.0, &mut span.1] {
                if *offset > start {
                    *offset = offset.wrapping_add_signed(grown);
                }
            }
        }
    }
}

impl Values {
    /// The record as a view.
    pub fn rec(&self) -> Rec<'_> {
        Rec(&self.0)
    }

    /// See [`Rec::get`].
    pub fn get(&self, id: u32) -> Option<&str> {
        self.rec().get(id)
    }

    /// See [`Rec::any`].
    pub fn any(&self, lo: u32, hi: u32) -> bool {
        self.rec().any(lo, hi)
    }

    /// See [`Rec::any_value`].
    pub fn any_value(&self, lo: u32, hi: u32) -> bool {
        self.rec().any_value(lo, hi)
    }

    /// See [`Rec::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.rec().is_empty()
    }

    /// See [`Rec::all`].
    pub fn all(&self, id: u32) -> impl Iterator<Item = &str> + Clone + '_ {
        self.rec().all(id)
    }

    /// See [`Rec::items`].
    pub fn items(&self, id: u32) -> impl Iterator<Item = Rec<'_>> + Clone + '_ {
        self.rec().items(id)
    }

    /// Appends `value` under `id` (another occurrence of a repeated scalar).
    pub fn push(&mut self, id: u32, value: &str) {
        self.push_raw(id, value.as_bytes());
    }

    /// Appends an entry with `payload` as it is (a copied entry).
    pub fn push_raw(&mut self, id: u32, payload: &[u8]) {
        put_varint(&mut self.0, id as usize);
        put_varint(&mut self.0, payload.len());
        self.0.extend_from_slice(payload);
    }

    /// Sets the value under `id`: replaces the first entry with that id, or
    /// appends one.
    pub fn set(&mut self, id: u32, value: &str) {
        match self.rec().raw_entries().find(|(i, _, _)| *i == id) {
            Some((_, (start, _), _)) => self.replace_at(start, id, value),
            None => self.push(id, value),
        }
    }

    /// Replaces the entry starting at `start` with `value` under `id`.
    fn replace_at(&mut self, start: usize, id: u32, value: &str) {
        let end = Entries {
            bytes: &self.0,
            at: start,
        }
        .next()
        .map_or(start, |(_, (_, end), _)| end);
        let mut entry = Vec::with_capacity(value.len() + 4);
        put_varint(&mut entry, id as usize);
        put_varint(&mut entry, value.len());
        entry.extend_from_slice(value.as_bytes());
        self.0.splice(start..end, entry);
    }

    /// Marks the interior element `id` present, even without content.
    pub fn mark(&mut self, id: u32) {
        if self.rec().entries().all(|(i, _)| i != id) {
            self.push(id, "");
        }
    }

    /// Nests a copy of `item` under `id`, after the items already there.
    pub fn push_record(&mut self, id: u32, item: Rec<'_>) {
        self.push_raw(id, item.0);
    }

    /// Starts a nested record under `id` that is appended to in place; pass
    /// the returned position to [`Values::end`] once it is complete.
    pub fn begin(&mut self, id: u32) -> usize {
        put_varint(&mut self.0, id as usize);
        let at = self.0.len();
        self.0.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
        at
    }

    /// Completes the nested record [`Values::begin`] started at `at`.
    pub fn end(&mut self, at: usize) {
        let len = self.0.len() - at - RESERVED;
        assert!(len < 1 << 35, "a nested record over 32 GiB");
        for (i, byte) in self.0[at..at + RESERVED].iter_mut().enumerate() {
            let bits = ((len >> (7 * i)) & 0x7f) as u8;
            *byte = if i + 1 < RESERVED { bits | 0x80 } else { bits };
        }
    }

    /// Releases unused capacity.
    pub fn shrink(&mut self) {
        self.0.shrink_to_fit();
    }

    /// Reserves room for `additional` more bytes.
    pub fn reserve(&mut self, additional: usize) {
        self.0.reserve(additional);
    }

    /// The record's size in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// A scalar a packed struct holds: how it is written as a payload and read
/// back. Text is its UTF-8 bytes, a boolean one byte, a decimal one byte of
/// scale and sign followed by its magnitude as a varint.
pub trait Field: Sized {
    /// Appends the value under `id`.
    fn put(&self, id: u32, out: &mut Values);
    /// The value a payload [`Field::put`] wrote holds.
    fn read(payload: &[u8]) -> Self;
}

impl Field for compact_str::CompactString {
    fn put(&self, id: u32, out: &mut Values) {
        out.push(id, self);
    }

    fn read(payload: &[u8]) -> Self {
        text(payload).into()
    }
}

impl Field for bool {
    fn put(&self, id: u32, out: &mut Values) {
        out.push_raw(id, &[u8::from(*self)]);
    }

    fn read(payload: &[u8]) -> Self {
        payload == [1]
    }
}

impl Field for rust_decimal::Decimal {
    fn put(&self, id: u32, out: &mut Values) {
        // A 96-bit magnitude takes at most 14 varint bytes.
        let mut buf = [0u8; 15];
        buf[0] = self.scale() as u8 | (u8::from(self.is_sign_negative()) << 7);
        let mut magnitude = self.mantissa().unsigned_abs();
        let mut n = 1;
        while magnitude >= 0x80 {
            buf[n] = (magnitude as u8) | 0x80;
            magnitude >>= 7;
            n += 1;
        }
        buf[n] = magnitude as u8;
        out.push_raw(id, &buf[..=n]);
    }

    fn read(payload: &[u8]) -> Self {
        let mut magnitude = 0u128;
        for (i, byte) in payload[1..].iter().enumerate() {
            magnitude |= u128::from(byte & 0x7f) << (7 * i);
        }
        let mut value = rust_decimal::Decimal::from_i128_with_scale(
            magnitude as i128,
            u32::from(payload[0] & 0x7f),
        );
        // A negative zero stays one.
        value.set_sign_negative(payload[0] & 0x80 != 0);
        value
    }
}

/// A struct kept as a record: the generated hub item structs, whose fields
/// take the ids `0..` in key order.
pub trait Pack {
    /// Appends the struct's fields to `out`.
    fn pack(&self, out: &mut Values);
    /// The struct [`Pack::pack`] wrote as `rec`.
    fn unpack(rec: Rec<'_>) -> Self;
}

/// A field of a [`Pack`] struct: an optional scalar or a list of nested
/// structs, written under its id and read back entry by entry.
pub trait Slot {
    /// Appends the field's entries under `id` (none when it is empty).
    fn put(&self, id: u32, out: &mut Values);
    /// Reads one entry of the field in.
    fn read(&mut self, payload: &[u8]);
}

impl<F: Field> Slot for Option<F> {
    fn put(&self, id: u32, out: &mut Values) {
        if let Some(value) = self {
            value.put(id, out);
        }
    }

    fn read(&mut self, payload: &[u8]) {
        *self = Some(F::read(payload));
    }
}

impl<T: Pack> Slot for Vec<T> {
    fn put(&self, id: u32, out: &mut Values) {
        for item in self {
            let at = out.begin(id);
            item.pack(out);
            out.end(at);
        }
    }

    fn read(&mut self, payload: &[u8]) {
        self.push(T::unpack(Rec(payload)));
    }
}

/// A list of [`Pack`] structs kept as one record, each item nested under id
/// 0: the hub's collections, which a struct per item would hold at many
/// times the size of their values. Items are decoded when they are read and
/// encoded when they are pushed; [`Packed::update`] changes them in place.
pub struct Packed<T> {
    values: Values,
    len: usize,
    item: std::marker::PhantomData<fn() -> T>,
}

impl<T> Default for Packed<T> {
    fn default() -> Self {
        Packed {
            values: Values::default(),
            len: 0,
            item: std::marker::PhantomData,
        }
    }
}

impl<T: Pack> Packed<T> {
    /// Appends `item`.
    pub fn push(&mut self, item: T) {
        let at = self.values.begin(0);
        item.pack(&mut self.values);
        self.values.end(at);
        self.len += 1;
    }

    /// Accepts the item count a reader knows in advance, as `Vec::reserve`.
    // ponytail: the bytes an item takes are not known ahead; a large buffer
    // grows by doubling, and pages it does not touch are never resident.
    pub fn reserve(&mut self, _additional: usize) {}

    /// The number of items.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there is no item.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The items, decoded one by one.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            entries: self.values.rec().raw_entries(),
            item: std::marker::PhantomData,
        }
    }

    /// The items as records, to read a field or two without decoding them
    /// ([`Rec::field`], [`Rec::get`]).
    pub fn records(&self) -> impl Iterator<Item = Rec<'_>> + '_ {
        self.values.rec().items(0)
    }

    /// Runs `f` on every item whose record `wanted` accepts; an item `f`
    /// changes is written back. `wanted` must accept every item `f` would
    /// change: the others are not decoded. The buffer is rewritten from the
    /// first changed item on, and not at all when nothing changes.
    pub fn update(&mut self, mut wanted: impl FnMut(Rec<'_>) -> bool, mut f: impl FnMut(&mut T)) {
        let mut changed: Option<Values> = None;
        let mut scratch = Values::default();
        for (_, (start, _), payload) in self.values.rec().raw_entries() {
            let entry: &[u8] = if wanted(Rec(payload)) {
                let mut item = T::unpack(Rec(payload));
                f(&mut item);
                scratch.0.clear();
                item.pack(&mut scratch);
                &scratch.0
            } else {
                payload
            };
            if changed.is_none() {
                // An item encodes one way only, so equal bytes are no change.
                if entry == payload {
                    continue;
                }
                changed = Some(Values(self.values.0[..start].to_vec()));
            }
            if let Some(out) = &mut changed {
                out.push_raw(0, entry);
            }
        }
        if let Some(out) = changed {
            self.values = out;
        }
    }
}

impl<T: Pack> FromIterator<T> for Packed<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut packed = Packed::default();
        for item in items {
            packed.push(item);
        }
        packed
    }
}

impl<T: Pack + fmt::Debug> fmt::Debug for Packed<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The items of a [`Packed`] list, decoded one by one.
pub struct Iter<'a, T> {
    entries: Entries<'a>,
    item: std::marker::PhantomData<fn() -> T>,
}

impl<T: Pack> Iterator for Iter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.entries
            .next()
            .map(|(_, _, payload)| T::unpack(Rec(payload)))
    }
}

/// The items of a [`Packed`] list taken by value, decoded one by one.
pub struct IntoIter<T> {
    values: Values,
    at: usize,
    item: std::marker::PhantomData<fn() -> T>,
}

impl<T: Pack> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        let mut entries = Entries {
            bytes: &self.values.0,
            at: self.at,
        };
        let (_, _, payload) = entries.next()?;
        let item = T::unpack(Rec(payload));
        self.at = entries.at;
        Some(item)
    }
}

impl<T: Pack> IntoIterator for Packed<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> IntoIter<T> {
        IntoIter {
            values: self.values,
            at: 0,
            item: std::marker::PhantomData,
        }
    }
}

impl fmt::Debug for Rec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for (id, payload) in self.entries() {
            match std::str::from_utf8(payload) {
                Ok(value) => list.entry(&(id, value)),
                Err(_) => list.entry(&(id, payload.len())),
            };
        }
        list.finish()
    }
}

impl fmt::Debug for Values {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.rec().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_all_and_set() {
        let mut v = Values::default();
        v.push(3, "a");
        v.push(200, "long id");
        v.push(3, "b");
        assert_eq!(v.get(3), Some("a"));
        assert_eq!(v.get(200), Some("long id"));
        assert_eq!(v.get(4), None);
        assert_eq!(v.rec().all(3).collect::<Vec<_>>(), ["a", "b"]);
        v.set(200, "x");
        v.set(5, "new");
        assert_eq!(v.get(200), Some("x"));
        assert_eq!(v.get(5), Some("new"));
        assert_eq!(v.rec().all(3).collect::<Vec<_>>(), ["a", "b"]);
        let long = "y".repeat(300);
        v.set(3, &long);
        assert_eq!(v.get(3), Some(long.as_str()));
        assert_eq!(v.rec().all(3).nth(1), Some("b"));
    }

    #[test]
    fn test_marks_and_ranges() {
        let mut v = Values::default();
        assert!(v.is_empty() && !v.any(0, 10));
        v.mark(4);
        v.mark(4);
        assert_eq!(v.len(), 2, "a second mark adds nothing");
        assert!(v.any(4, 6) && !v.any_value(4, 6) && v.is_empty());
        v.push(5, "");
        assert!(!v.any_value(0, 10), "an empty value has no content");
        v.push(5, "x");
        assert!(v.any_value(5, 6) && !v.any_value(6, 9) && !v.is_empty());
    }

    #[test]
    fn test_nested_items_in_place_and_pushed() {
        let mut root = Values::default();
        root.push(1, "head");
        for name in ["one", "two"] {
            let at = root.begin(7);
            root.push(0, name);
            let inner = root.begin(2);
            root.push(0, "deep");
            root.end(inner);
            root.end(at);
        }
        let mut third = Values::default();
        third.push(0, "three");
        root.push_record(7, third.rec());
        root.push(9, "tail");
        let names: Vec<_> = root.rec().items(7).map(|i| i.get(0).unwrap()).collect();
        assert_eq!(names, ["one", "two", "three"]);
        let first = root.rec().items(7).next().unwrap();
        assert_eq!(first.items(2).next().unwrap().get(0), Some("deep"));
        assert_eq!(root.get(9), Some("tail"));
        assert_eq!(root.get(0), None, "nested ids are not the root's");
        assert!(root.any_value(7, 8));
        let copy = root.rec().extract(7, 10);
        assert_eq!(copy.rec().items(0).count(), 3);
        assert_eq!(copy.get(2), Some("tail"));
    }

    #[test]
    fn test_index_answers_as_the_record() {
        let mut v = Values::default();
        v.push(1, "a");
        v.push_record(4, Rec(b"\x00\x01x"));
        v.push(2, "b");
        v.push_record(4, Rec(b""));
        v.push(1, "again");
        v.push(6, "");
        let index = Index::new(v.rec(), 8);
        for id in 0..8 {
            assert_eq!(index.get(id), v.get(id), "get({id})");
            assert!(index.all(id).eq(v.rec().all(id)), "all({id})");
            assert!(index.items(id).eq(v.rec().items(id)), "items({id})");
            for hi in id..9 {
                assert_eq!(index.any(id, hi), v.any(id, hi), "any({id}, {hi})");
            }
        }
        assert_eq!(index.items(4).next().and_then(|i| i.get(0)), Some("x"));
    }

    #[test]
    fn test_draft_builds_what_values_build() {
        let mut d = Draft::new(8);
        let mut v = Values::default();
        let long = "z".repeat(200);
        for (id, value) in [
            (3, "a"),
            (1, "b"),
            (3, "c"),
            (5, ""),
            (1, long.as_str()),
            (6, "d"),
            (3, ""),
        ] {
            d.set(id, value);
            v.set(id, value);
            assert_eq!(d.rec(), v.rec(), "set({id}, {value:?})");
        }
        d.push(2, "p");
        v.push(2, "p");
        d.push_record(7, Rec(b"\x00\x01x"));
        v.push_record(7, Rec(b"\x00\x01x"));
        d.mark(4);
        d.mark(4);
        v.mark(4);
        d.set(1, "short");
        v.set(1, "short");
        assert_eq!(d.rec(), v.rec());
        for id in 0..8 {
            assert_eq!(d.get(id), v.get(id), "get({id})");
            assert!(d.items(id).eq(v.rec().items(id)), "items({id})");
        }
        assert_eq!(d.into_values(), v);
    }

    #[derive(Clone, Debug, Default, PartialEq)]
    struct Line {
        id: Option<compact_str::CompactString>,
        amount: Option<rust_decimal::Decimal>,
        flag: Option<bool>,
        subs: Vec<Line>,
    }

    impl Pack for Line {
        fn pack(&self, out: &mut Values) {
            self.id.put(0, out);
            self.amount.put(1, out);
            self.flag.put(2, out);
            self.subs.put(3, out);
        }

        fn unpack(rec: Rec<'_>) -> Self {
            let mut line = Line::default();
            for (id, payload) in rec.entries() {
                match id {
                    0 => line.id.read(payload),
                    1 => line.amount.read(payload),
                    2 => line.flag.read(payload),
                    3 => line.subs.read(payload),
                    _ => {}
                }
            }
            line
        }
    }

    #[test]
    fn test_packed_round_trips_and_updates() {
        use std::str::FromStr as _;
        let dec = |s| rust_decimal::Decimal::from_str(s).unwrap();
        let line = |id: &str, amount| Line {
            id: Some(id.into()),
            amount,
            ..Line::default()
        };
        let mut neg_zero = dec("0.00");
        neg_zero.set_sign_negative(true);
        let amounts = [
            dec("100.00"),
            dec("-12.5"),
            neg_zero,
            rust_decimal::Decimal::MAX,
            rust_decimal::Decimal::MIN,
            dec("0.0000000000000000000000000001"),
        ];
        let mut lines: Vec<Line> = amounts.iter().map(|a| line("x", Some(*a))).collect();
        lines.push(Line {
            id: Some("".into()),
            flag: Some(false),
            subs: vec![line("sub", None), Line::default()],
            ..Line::default()
        });
        lines.push(Line::default());
        let mut packed: Packed<Line> = lines.iter().cloned().collect();
        assert_eq!(packed.len(), lines.len());
        let back: Vec<Line> = packed.iter().collect();
        assert_eq!(back, lines);
        for (a, b) in back.iter().zip(&lines) {
            assert_eq!(
                format!("{:?}", a.amount),
                format!("{:?}", b.amount),
                "scale and sign kept"
            );
        }
        let before = packed.values.clone();
        packed.update(|_| true, |_| {});
        assert_eq!(packed.values, before, "no change, no rewrite");
        let flagged = |r: Rec<'_>| r.field::<bool>(2) == Some(false);
        assert_eq!(packed.records().filter(|r| flagged(*r)).count(), 1);
        packed.update(flagged, |l| l.subs[0].flag = Some(true));
        lines[6].subs[0].flag = Some(true);
        packed.update(|r| r.get(0) == Some("x"), |l| l.amount = None);
        for line in &mut lines[..6] {
            line.amount = None;
        }
        assert!(packed.iter().eq(lines.iter().cloned()));
        assert_eq!(packed.into_iter().collect::<Vec<_>>(), lines);
    }
}
