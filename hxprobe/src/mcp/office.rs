//! Text of Word, Excel and PowerPoint attachments (`.docx`, `.xlsx`,
//! `.pptx`).
//!
//! The three formats are ZIP archives of XML. This module finds an entry in
//! the archive (end record and central directory), inflates it with
//! `miniz_oxide`, and pulls the text out of the XML. The input comes from
//! mail, so every offset and length is checked, every allocation stays under
//! a stated limit, and every failure is an error value, never a panic.

use std::borrow::Cow;

/// Most entries an archive may have.
pub const MAX_ENTRIES: usize = 10_000;
/// Most bytes one entry may inflate to.
pub const MAX_ENTRY_BYTES: usize = 50_000_000;
/// Most bytes all entries read in one call may inflate to together.
pub const MAX_INFLATED: usize = 150_000_000;

/// Which of the three formats an archive is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeKind {
    Docx,
    Xlsx,
    Pptx,
}

/// Signature of the end of central directory record.
const END_SIG: u32 = 0x0605_4b50;
/// Signature of the ZIP64 end of central directory locator.
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
/// Signature of a central directory header.
const CENTRAL_SIG: u32 = 0x0201_4b50;
/// Signature of a local file header.
const LOCAL_SIG: u32 = 0x0403_4b50;
/// Length of the end record without its comment.
const END_LEN: usize = 22;
/// Length of a central directory header without its variable fields.
const CENTRAL_LEN: usize = 46;
/// Length of a local header without its variable fields.
const LOCAL_LEN: usize = 30;
/// The end record lies within this many bytes of the end (its own length
/// plus the longest comment).
const END_SEARCH: usize = END_LEN + 0xffff;
/// Flag bits of an encrypted entry (traditional and strong encryption).
const ENCRYPTED: u16 = 0x0001 | 0x0040;

/// One entry of the central directory.
struct Record<'a> {
    name: &'a [u8],
    flags: u16,
    method: u16,
    /// Bytes of the entry as stored in the archive.
    packed: usize,
    /// Bytes of the entry once inflated, as recorded.
    size: usize,
    /// Offset of the entry's local header.
    local: usize,
}

/// Little-endian `u16` at `at`, if `b` is long enough.
fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// Little-endian `u32` at `at`, if `b` is long enough.
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Offset of the end record: the last signature in the last [`END_SEARCH`]
/// bytes whose comment fits in what follows.
fn end_record(archive: &[u8]) -> Option<usize> {
    let last = archive.len().checked_sub(END_LEN)?;
    let first = archive.len().saturating_sub(END_SEARCH);
    (first..=last).rev().find(|&at| {
        u32_at(archive, at) == Some(END_SIG)
            && u16_at(archive, at + 20)
                .is_some_and(|c| at + END_LEN + usize::from(c) <= archive.len())
    })
}

/// The central directory of `archive`, every offset and length checked. A
/// ZIP64 archive, an archive on several disks and one with more than
/// [`MAX_ENTRIES`] entries are errors.
fn directory(archive: &[u8]) -> Result<Vec<Record<'_>>, ()> {
    let end = end_record(archive).ok_or(())?;
    // A ZIP64 archive has its locator right before the end record.
    if end >= 20 && u32_at(archive, end - 20) == Some(ZIP64_LOCATOR_SIG) {
        return Err(());
    }
    let field16 = |at| u16_at(archive, end + at).ok_or(());
    let field32 = |at| u32_at(archive, end + at).ok_or(());
    let (disk, cd_disk) = (field16(4)?, field16(6)?);
    let (on_disk, total) = (field16(8)?, field16(10)?);
    let (cd_size, cd_offset) = (field32(12)?, field32(16)?);
    // All-ones fields point at a ZIP64 record.
    if on_disk == 0xffff || total == 0xffff || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(());
    }
    if disk != 0 || cd_disk != 0 || on_disk != total {
        return Err(());
    }
    let count = usize::from(total);
    if count > MAX_ENTRIES {
        return Err(());
    }
    let start = cd_offset as usize;
    let cd_end = start.checked_add(cd_size as usize).ok_or(())?;
    if cd_end > end {
        return Err(());
    }
    let cd = &archive[start..cd_end];
    let mut records = Vec::with_capacity(count);
    let mut at = 0usize;
    for _ in 0..count {
        if u32_at(cd, at) != Some(CENTRAL_SIG) {
            return Err(());
        }
        let f16 = |o: usize| u16_at(cd, at + o).map(usize::from).ok_or(());
        let f32 = |o: usize| u32_at(cd, at + o).ok_or(());
        let (flags, method) = (f16(8)?, f16(10)?);
        let (packed, size, local) = (f32(20)?, f32(24)?, f32(42)?);
        if packed == u32::MAX || size == u32::MAX || local == u32::MAX {
            return Err(());
        }
        let (name_len, extra_len, comment_len) = (f16(28)?, f16(30)?, f16(32)?);
        let name_start = at + CENTRAL_LEN;
        let name = cd.get(name_start..name_start + name_len).ok_or(())?;
        let next = name_start + name_len + extra_len + comment_len;
        if next > cd.len() || local as usize >= start {
            return Err(());
        }
        records.push(Record {
            name,
            flags: flags as u16,
            method: method as u16,
            packed: packed as usize,
            size: size as usize,
            local: local as usize,
        });
        at = next;
    }
    Ok(records)
}

/// The bytes of `r`, inflated, under `limit` bytes.
fn read(archive: &[u8], r: &Record, limit: usize) -> Result<Vec<u8>, ()> {
    if r.flags & ENCRYPTED != 0 || r.size > limit {
        return Err(());
    }
    if u32_at(archive, r.local) != Some(LOCAL_SIG) {
        return Err(());
    }
    let name_len = usize::from(u16_at(archive, r.local + 26).ok_or(())?);
    let extra_len = usize::from(u16_at(archive, r.local + 28).ok_or(())?);
    let data = r.local + LOCAL_LEN + name_len + extra_len;
    let data = archive
        .get(data..data.checked_add(r.packed).ok_or(())?)
        .ok_or(())?;
    match r.method {
        0 if data.len() == r.size => Ok(data.to_vec()),
        // Inflating stops at the recorded size (one byte for an empty
        // entry, so that a stream that gives anything is seen), so a stream
        // that gives more never takes more memory than the record admits.
        8 => match miniz_oxide::inflate::decompress_to_vec_with_limit(data, r.size.max(1)) {
            Ok(out) if out.len() == r.size => Ok(out),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

/// The names of the entries of `archive`, as found in the central directory.
fn names(archive: &[u8]) -> Result<Vec<&[u8]>, ()> {
    Ok(directory(archive)?.into_iter().map(|r| r.name).collect())
}

/// The kind of `archive`, from the names in its central directory alone
/// (nothing is inflated); `None` for an archive that is none of the three
/// or no archive at all.
pub fn kind_of(archive: &[u8]) -> Option<OfficeKind> {
    let names = names(archive).ok()?;
    let has = |n: &str| names.contains(&n.as_bytes());
    if has("word/document.xml") {
        Some(OfficeKind::Docx)
    } else if has("xl/workbook.xml") {
        Some(OfficeKind::Xlsx)
    } else if has("ppt/presentation.xml") {
        Some(OfficeKind::Pptx)
    } else {
        None
    }
}

/// The bytes of the entry `name` of `archive`, inflated; `None` when there is
/// no such entry. What it inflates to is taken from `budget`.
// Used by the readers of the next steps; until then by the tests only.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entry(archive: &[u8], name: &str, budget: &mut usize) -> Result<Option<Vec<u8>>, ()> {
    entry_with(archive, name, budget, MAX_ENTRY_BYTES)
}

/// [`entry`] with a given limit per entry.
fn entry_with(
    archive: &[u8],
    name: &str,
    budget: &mut usize,
    max_entry: usize,
) -> Result<Option<Vec<u8>>, ()> {
    let records = directory(archive)?;
    let Some(r) = records.iter().find(|r| r.name == name.as_bytes()) else {
        return Ok(None);
    };
    let out = read(archive, r, max_entry.min(*budget))?;
    *budget -= out.len();
    Ok(Some(out))
}

/// One event of the XML scanner.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event<'a> {
    /// A start tag; an empty tag (`<a/>`) is a start tag followed by its end.
    Start(Tag<'a>),
    /// An end tag, by its name.
    End(&'a [u8]),
    /// Text or CDATA, entities decoded.
    Text(Cow<'a, str>),
}

/// A start tag: its name (prefix included, as written) and its attributes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Tag<'a> {
    pub name: &'a [u8],
    /// The bytes between the name and the end of the tag.
    attrs: &'a [u8],
}

impl<'a> Tag<'a> {
    /// The decoded value of the attribute `name`, if the tag has it; `None`
    /// also once the attributes stop being well formed.
    // Used by the readers of the next steps; until then by the tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn attr(&self, name: &str) -> Option<Cow<'a, str>> {
        let b = self.attrs;
        let mut i = 0;
        loop {
            while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                i += 1;
            }
            let start = i;
            while b
                .get(i)
                .is_some_and(|c| !c.is_ascii_whitespace() && !matches!(c, b'=' | b'/'))
            {
                i += 1;
            }
            let key = &b[start..i];
            while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                i += 1;
            }
            if key.is_empty() || b.get(i) != Some(&b'=') {
                return None;
            }
            i += 1;
            while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                i += 1;
            }
            let quote = *b.get(i).filter(|q| matches!(q, b'"' | b'\''))?;
            let value_start = i + 1;
            let value_end = value_start + memchr::memchr(quote, b.get(value_start..)?)?;
            i = value_end + 1;
            if key == name.as_bytes() {
                return Some(decode(&b[value_start..value_end]));
            }
        }
    }
}

/// Longest entity looked at after `&`, `;` included.
const MAX_ENTITY_LEN: usize = 32;

/// Text of XML bytes: invalid UTF-8 replaced, the five named entities and
/// numeric ones decoded. An unknown or unterminated entity stays as written;
/// an invalid numeric one becomes the replacement character.
fn decode(b: &[u8]) -> Cow<'_, str> {
    let text = String::from_utf8_lossy(b);
    if !text.contains('&') {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut rest: &str = &text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let window = &rest.as_bytes()[..rest.len().min(MAX_ENTITY_LEN)];
        let Some(semi) = memchr::memchr(b';', window) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..semi];
        let c = match name {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name.strip_prefix('#').map(numeric),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The character of a numeric entity after its `#`; the replacement
/// character for one that is not a character XML allows.
fn numeric(digits: &str) -> char {
    let (digits, radix) = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (digits, 10),
    };
    let value = if digits.is_empty() {
        None
    } else {
        digits.chars().try_fold(0u32, |v, c| {
            v.checked_mul(radix)?.checked_add(c.to_digit(radix)?)
        })
    };
    value
        .filter(|v| *v != 0)
        .and_then(char::from_u32)
        .unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// An XML scanner over bytes: an iterator of [`Event`]s, without recursion
/// and linear in its input. A document type declaration is an error (so no
/// entity is ever expanded); input that ends inside markup ends the events.
pub(crate) struct Xml<'a> {
    b: &'a [u8],
    at: usize,
    /// The end event of an empty tag, given after its start.
    pending: Option<&'a [u8]>,
    done: bool,
}

impl<'a> Xml<'a> {
    // Used by the readers of the next steps; until then by the tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(b: &'a [u8]) -> Xml<'a> {
        Xml {
            b,
            at: 0,
            pending: None,
            done: false,
        }
    }
}

/// Offset in `b` of the `>` that ends a start tag, outside quoted values.
fn tag_end(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    loop {
        i += memchr::memchr3(b'>', b'"', b'\'', b.get(i..)?)?;
        match b[i] {
            b'>' => return Some(i),
            q => i += 1 + memchr::memchr(q, b.get(i + 1..)?)? + 1,
        }
    }
}

/// `b` without trailing ASCII whitespace.
fn trim_end(b: &[u8]) -> &[u8] {
    let n = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    &b[..n]
}

impl<'a> Iterator for Xml<'a> {
    type Item = Result<Event<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(name) = self.pending.take() {
            return Some(Ok(Event::End(name)));
        }
        loop {
            if self.done {
                return None;
            }
            let rest = self.b.get(self.at..).filter(|r| !r.is_empty())?;
            if rest[0] != b'<' {
                let n = memchr::memchr(b'<', rest).unwrap_or(rest.len());
                self.at += n;
                return Some(Ok(Event::Text(decode(&rest[..n]))));
            }
            // Markup whose end is never found ends the events: nothing after
            // it can be read either, and stopping keeps the scan linear.
            let skip_to = |from: usize, end: &[u8]| {
                memchr::memmem::find(rest.get(from..)?, end).map(|i| from + i)
            };
            if rest.starts_with(b"<?") || rest.starts_with(b"<!--") {
                let (from, end): (usize, &[u8]) = if rest[1] == b'?' {
                    (2, b"?>")
                } else {
                    (4, b"-->")
                };
                match skip_to(from, end) {
                    Some(i) => self.at += i + end.len(),
                    None => self.done = true,
                }
                continue;
            }
            if rest.starts_with(b"<![CDATA[") {
                let Some(i) = skip_to(9, b"]]>") else {
                    self.done = true;
                    continue;
                };
                self.at += i + 3;
                return Some(Ok(Event::Text(String::from_utf8_lossy(&rest[9..i]))));
            }
            if rest.starts_with(b"<!") {
                // A document type declaration, or another declaration.
                self.done = true;
                return Some(Err(()));
            }
            if rest.starts_with(b"</") {
                let Some(i) = memchr::memchr(b'>', rest) else {
                    self.done = true;
                    continue;
                };
                self.at += i + 1;
                return Some(Ok(Event::End(trim_end(&rest[2..i]))));
            }
            let Some(i) = tag_end(rest) else {
                self.done = true;
                continue;
            };
            self.at += i + 1;
            let mut inner = &rest[1..i];
            let empty = inner.last() == Some(&b'/');
            if empty {
                inner = &inner[..inner.len() - 1];
            }
            let n = inner
                .iter()
                .position(|c| c.is_ascii_whitespace() || *c == b'/')
                .unwrap_or(inner.len());
            let (name, attrs) = inner.split_at(n);
            if empty {
                self.pending = Some(name);
            }
            return Some(Ok(Event::Start(Tag { name, attrs })));
        }
    }
}

#[cfg(test)]
pub(crate) mod testzip {
    //! Archives built in memory for tests.

    /// One entry as it is written: `data` is already in its stored form.
    pub struct Raw<'a> {
        pub name: &'a str,
        pub data: Vec<u8>,
        pub method: u16,
        pub flags: u16,
        /// The recorded inflated size.
        pub size: u32,
    }

    /// An entry stored (`deflated` false) or deflated, with its true size.
    pub fn raw<'a>(name: &'a str, bytes: &[u8], deflated: bool) -> Raw<'a> {
        Raw {
            name,
            data: if deflated {
                miniz_oxide::deflate::compress_to_vec(bytes, 6)
            } else {
                bytes.to_vec()
            },
            method: if deflated { 8 } else { 0 },
            flags: 0,
            size: bytes.len() as u32,
        }
    }

    /// An archive of `(name, bytes, deflated)` entries.
    pub fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let raws: Vec<Raw> = entries.iter().map(|(n, b, d)| raw(n, b, *d)).collect();
        zip_raw(&raws)
    }

    /// An archive of entries as given: local headers, central directory, end
    /// record. CRCs are left 0 (the reader does not check them).
    pub fn zip_raw(entries: &[Raw]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for e in entries {
            let local = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&e.flags.to_le_bytes());
            out.extend_from_slice(&e.method.to_le_bytes());
            out.extend_from_slice(&[0; 4]); // time, date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&e.size.to_le_bytes());
            out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(e.name.as_bytes());
            out.extend_from_slice(&e.data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // made by
            central.extend_from_slice(&20u16.to_le_bytes()); // needed
            central.extend_from_slice(&e.flags.to_le_bytes());
            central.extend_from_slice(&e.method.to_le_bytes());
            central.extend_from_slice(&[0; 4]); // time, date
            central.extend_from_slice(&0u32.to_le_bytes()); // crc
            central.extend_from_slice(&(e.data.len() as u32).to_le_bytes());
            central.extend_from_slice(&e.size.to_le_bytes());
            central.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 4]); // extra, comment
            central.extend_from_slice(&[0; 4]); // disk, internal attributes
            central.extend_from_slice(&[0; 4]); // external attributes
            central.extend_from_slice(&local.to_le_bytes());
            central.extend_from_slice(e.name.as_bytes());
        }
        let offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // disk numbers
        let n = (entries.len() as u16).to_le_bytes();
        out.extend_from_slice(&n);
        out.extend_from_slice(&n);
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out
    }

    /// A deflate stream (fixed Huffman codes, written bit by bit) of one
    /// zero byte followed by `matches` copies of 258 zero bytes: about 13
    /// bits per 258 bytes, so a few hundred kilobytes stand for gigabytes.
    pub fn zero_bomb(matches: usize) -> Vec<u8> {
        struct Bits {
            out: Vec<u8>,
            acc: u32,
            n: u32,
        }
        impl Bits {
            /// `n` bits of `v`, least significant first.
            fn put(&mut self, v: u32, n: u32) {
                for i in 0..n {
                    self.acc |= ((v >> i) & 1) << self.n;
                    self.n += 1;
                    if self.n == 8 {
                        self.out.push(self.acc as u8);
                        self.acc = 0;
                        self.n = 0;
                    }
                }
            }
            /// A Huffman code of `n` bits, most significant first.
            fn code(&mut self, v: u32, n: u32) {
                for i in (0..n).rev() {
                    self.put((v >> i) & 1, 1);
                }
            }
        }
        let mut b = Bits {
            out: Vec::new(),
            acc: 0,
            n: 0,
        };
        b.put(1, 1); // last block
        b.put(1, 2); // fixed Huffman codes
        b.code(0x30, 8); // literal 0
        for _ in 0..matches {
            b.code(0xC5, 8); // length 258 (symbol 285)
            b.code(0, 5); // distance 1
        }
        b.code(0, 7); // end of block
        if b.n > 0 {
            b.out.push(b.acc as u8);
        }
        b.out
    }
}

#[cfg(test)]
mod tests {
    use super::testzip::{raw, zero_bomb, zip, zip_raw, Raw};
    use super::*;

    /// [`entry`] with the budget of one call.
    fn get(archive: &[u8], name: &str) -> Result<Option<Vec<u8>>, ()> {
        let mut budget = MAX_INFLATED;
        entry(archive, name, &mut budget)
    }

    #[test]
    fn stored_and_deflated_entries_are_found() {
        let text = "<w:document>שלום</w:document>".repeat(100);
        let a = zip(&[
            ("a.txt", b"plain bytes", false),
            ("word/document.xml", text.as_bytes(), true),
            ("empty", b"", true),
        ]);
        assert_eq!(get(&a, "a.txt"), Ok(Some(b"plain bytes".to_vec())));
        assert_eq!(get(&a, "word/document.xml"), Ok(Some(text.into_bytes())));
        assert_eq!(get(&a, "empty"), Ok(Some(Vec::new())));
        assert_eq!(get(&a, "missing.xml"), Ok(None));
        assert_eq!(get(&a, "A.TXT"), Ok(None));
    }

    #[test]
    fn deflated_entries_of_many_sizes_inflate_exactly() {
        for n in [1usize, 2, 257, 65_535, 65_536, 100_001] {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            let a = zip(&[("x", &bytes, true)]);
            assert_eq!(get(&a, "x"), Ok(Some(bytes)), "size {n}");
        }
    }

    #[test]
    fn the_kind_comes_from_the_marker_entry() {
        let k = |name: &str| {
            kind_of(&zip(&[
                ("[Content_Types].xml", b"x", true),
                (name, b"<x/>", true),
            ]))
        };
        assert_eq!(k("word/document.xml"), Some(OfficeKind::Docx));
        assert_eq!(k("xl/workbook.xml"), Some(OfficeKind::Xlsx));
        assert_eq!(k("ppt/presentation.xml"), Some(OfficeKind::Pptx));
        assert_eq!(k("other.xml"), None);
        assert_eq!(k("word/document.xml.bak"), None);
        assert_eq!(kind_of(&zip(&[])), None);
        assert_eq!(kind_of(b"not an archive"), None);
        assert_eq!(kind_of(b""), None);
        // The names alone decide: a marker that does not inflate still counts.
        let bad = zip_raw(&[Raw {
            name: "word/document.xml",
            data: vec![0xff; 8],
            method: 8,
            flags: 0,
            size: 10,
        }]);
        assert_eq!(kind_of(&bad), Some(OfficeKind::Docx));
    }

    /// Offset of the end record in an archive without a comment.
    fn eocd(a: &[u8]) -> usize {
        a.len() - 22
    }

    #[test]
    fn broken_archives_are_refused() {
        let good = zip(&[("a", b"0123456789", false), ("b", &[7; 300], true)]);
        assert!(get(&good, "b").is_ok());
        // Cut short anywhere: an error or, before the end record is lost, no
        // panic either way.
        for cut in 0..good.len() {
            let _ = get(&good[..cut], "b");
            let _ = kind_of(&good[..cut]);
        }
        assert_eq!(get(&good[..good.len() - 1], "b"), Err(()));
        assert_eq!(get(&good[..10], "a"), Err(()));
        assert_eq!(get(b"", "a"), Err(()));
        // No end record.
        let mut a = good.clone();
        let e = eocd(&a);
        a[e] = b'X';
        assert_eq!(get(&a, "a"), Err(()));
        // Central directory offset past the end.
        let mut a = good.clone();
        let e = eocd(&a);
        a[e + 16..e + 20].copy_from_slice(&(good.len() as u32 + 5).to_le_bytes());
        assert_eq!(get(&a, "a"), Err(()));
        // Central directory size past the end record.
        let mut a = good.clone();
        let e = eocd(&a);
        a[e + 12..e + 16].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert_eq!(get(&a, "a"), Err(()));
        // More entries recorded than the directory holds.
        let mut a = good.clone();
        let e = eocd(&a);
        a[e + 8..e + 12].copy_from_slice(&[3, 0, 3, 0]);
        assert_eq!(get(&a, "zzz"), Err(()));
    }

    #[test]
    fn a_local_header_offset_past_the_end_is_refused() {
        let good = zip(&[("a", b"0123456789", false)]);
        let central = good.len() - 22 - 46 - 1;
        let mut a = good.clone();
        a[central + 42..central + 46].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(get(&a, "a"), Err(()));
        let mut a = good.clone();
        a[central + 42..central + 46].copy_from_slice(&(good.len() as u32 - 10).to_le_bytes());
        assert_eq!(get(&a, "a"), Err(()));
        // Data that runs past the end of the archive.
        let mut a = good.clone();
        a[central + 20..central + 24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(get(&a, "a"), Err(()));
    }

    #[test]
    fn a_recorded_size_that_differs_is_refused() {
        let bytes = vec![b'x'; 5_000];
        for size in [0u32, 1, 4_999, 5_001, 1_000_000] {
            let mut e = raw("x", &bytes, true);
            e.size = size;
            assert_eq!(get(&zip_raw(&[e]), "x"), Err(()), "deflated, size {size}");
            let mut e = raw("x", &bytes, false);
            e.size = size;
            assert_eq!(get(&zip_raw(&[e]), "x"), Err(()), "stored, size {size}");
        }
    }

    #[test]
    fn more_than_ten_thousand_entries_are_refused() {
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("{i}")).collect();
        let entries: Vec<(&str, &[u8], bool)> = names
            .iter()
            .map(|n| (n.as_str(), &b""[..], false))
            .collect();
        let a = zip(&entries);
        assert_eq!(get(&a, "0"), Err(()));
        assert_eq!(kind_of(&a), None);
        let a = zip(&entries[..MAX_ENTRIES]);
        assert_eq!(get(&a, "0"), Ok(Some(Vec::new())));
    }

    #[test]
    fn encrypted_entries_and_other_methods_are_refused() {
        for flags in [1u16, 0x40, 0x41] {
            let mut e = raw("x", b"secret", false);
            e.flags = flags;
            assert_eq!(get(&zip_raw(&[e]), "x"), Err(()), "flags {flags}");
        }
        for method in [1u16, 9, 12, 14, 93, 99] {
            let mut e = raw("x", b"data", false);
            e.method = method;
            assert_eq!(get(&zip_raw(&[e]), "x"), Err(()), "method {method}");
        }
    }

    #[test]
    fn zip64_archives_are_refused() {
        let good = zip(&[("a", b"0123456789", false)]);
        // A ZIP64 end locator right before the end record.
        let e = eocd(&good);
        let mut a = good[..e].to_vec();
        a.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        a.extend_from_slice(&[0; 16]);
        a.extend_from_slice(&good[e..]);
        assert_eq!(get(&a, "a"), Err(()));
        // Fields that say "see the ZIP64 record".
        for (at, len) in [(8, 2), (10, 2), (12, 4), (16, 4)] {
            let mut a = good.clone();
            let e = eocd(&a);
            a[e + at..e + at + len].fill(0xff);
            assert_eq!(get(&a, "a"), Err(()), "field at {at}");
        }
        // An entry whose sizes or offset say the same.
        let central = good.len() - 22 - 46 - 1;
        for at in [20, 24, 42] {
            let mut a = good.clone();
            a[central + at..central + at + 4].fill(0xff);
            assert_eq!(get(&a, "a"), Err(()), "entry field at {at}");
        }
    }

    #[test]
    fn an_end_record_with_a_comment_is_found() {
        let mut a = zip(&[("a", b"0123456789", false)]);
        let e = eocd(&a);
        let comment = vec![b'c'; 65_535];
        a[e + 20..e + 22].copy_from_slice(&65_535u16.to_le_bytes());
        a.extend_from_slice(&comment);
        assert_eq!(get(&a, "a"), Ok(Some(b"0123456789".to_vec())));
        // A comment longer than what follows is no end record.
        a.pop();
        assert_eq!(get(&a, "a"), Err(()));
    }

    #[test]
    fn the_zero_bomb_inflates_to_zeros() {
        let bomb = zero_bomb(10);
        let out = miniz_oxide::inflate::decompress_to_vec_with_limit(&bomb, 10_000).unwrap();
        assert_eq!(out, vec![0; 1 + 10 * 258]);
    }

    #[test]
    fn a_zip_bomb_is_refused_without_inflating_it() {
        // 1,000,000 matches are 258,000,001 bytes of zeros in about 1.6 MB.
        let matches = 1_000_000;
        let bomb = zero_bomb(matches);
        assert!(bomb.len() < 2_000_000);
        let entry = |size: u32| {
            zip_raw(&[Raw {
                name: "word/document.xml",
                data: bomb.clone(),
                method: 8,
                flags: 0,
                size,
            }])
        };
        // A small recorded size: inflating stops at it.
        assert_eq!(get(&entry(1_000), "word/document.xml"), Err(()));
        // A huge recorded size: refused before anything is inflated.
        assert_eq!(get(&entry(u32::MAX - 1), "word/document.xml"), Err(()));
        assert_eq!(
            get(&entry(MAX_ENTRY_BYTES as u32 + 1), "word/document.xml"),
            Err(())
        );
        // Its true size is above the entry limit: refused before inflating.
        assert_eq!(
            get(&entry(1 + matches as u32 * 258), "word/document.xml"),
            Err(())
        );
    }

    #[test]
    fn an_entry_past_the_entry_limit_is_refused() {
        let bomb = zero_bomb(100);
        let size = 1 + 100 * 258;
        let a = zip_raw(&[Raw {
            name: "x",
            data: bomb.clone(),
            method: 8,
            flags: 0,
            size,
        }]);
        let mut budget = MAX_INFLATED;
        assert!(entry_with(&a, "x", &mut budget, size as usize).is_ok());
        assert_eq!(entry_with(&a, "x", &mut budget, size as usize - 1), Err(()));
        // A stored entry the same.
        let a = zip(&[("x", &[1; 1_000], false)]);
        assert_eq!(entry_with(&a, "x", &mut budget, 999), Err(()));
        assert!(entry_with(&a, "x", &mut budget, 1_000).is_ok());
    }

    #[test]
    fn the_call_budget_is_shared_by_all_entries() {
        let a = zip(&[("a", &[1; 600], true), ("b", &[2; 600], false)]);
        let mut budget = 1_000;
        assert_eq!(entry(&a, "a", &mut budget), Ok(Some(vec![1; 600])));
        assert_eq!(budget, 400);
        assert_eq!(entry(&a, "b", &mut budget), Err(()));
        assert_eq!(budget, 400);
        // A missing entry costs nothing.
        assert_eq!(entry(&a, "c", &mut budget), Ok(None));
        assert_eq!(budget, 400);
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Every single-byte change of a small archive.
        let good = zip(&[
            ("word/document.xml", &[b'a'; 50], true),
            ("b", b"xyz", false),
        ]);
        for at in 0..good.len() {
            for v in [0u8, 1, 0x7f, 0x80, 0xff] {
                let mut a = good.clone();
                a[at] = v;
                let _ = get(&a, "word/document.xml");
                let _ = get(&a, "b");
                let _ = kind_of(&a);
            }
        }
    }
}

#[cfg(test)]
mod xml_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The events of `xml` in a short form: `<name`, `>name` (end) and the
    /// text; `!` for an error (the last event then).
    fn events(xml: &str) -> Vec<String> {
        Xml::new(xml.as_bytes())
            .map(|e| match e {
                Ok(Event::Start(t)) => format!("<{}", String::from_utf8_lossy(t.name)),
                Ok(Event::End(n)) => format!(">{}", String::from_utf8_lossy(n)),
                Ok(Event::Text(t)) => t.into_owned(),
                Err(()) => "!".to_string(),
            })
            .collect()
    }

    /// The first start tag of `xml`.
    fn first_tag(xml: &str) -> Tag<'_> {
        Xml::new(xml.as_bytes())
            .find_map(|e| match e {
                Ok(Event::Start(t)) => Some(t),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn tags_with_and_without_attributes() {
        assert_eq!(
            events("<a><b x=\"1\">t</b><c/></a>"),
            ["<a", "<b", "t", ">b", "<c", ">c", ">a"]
        );
        assert_eq!(
            events("<w:p><w:r><w:t xml:space='preserve'> x </w:t></w:r></w:p>"),
            ["<w:p", "<w:r", "<w:t", " x ", ">w:t", ">w:r", ">w:p"]
        );
        assert_eq!(events("<a:t>s</a:t >"), ["<a:t", "s", ">a:t"]);
        assert_eq!(events("<x\n\tk = \"v\"\n/>"), ["<x", ">x"]);
        assert_eq!(events("plain"), ["plain"]);
        assert!(events("").is_empty());
    }

    #[test]
    fn attributes_in_single_and_double_quotes() {
        let t = first_tag("<c r=\"B7\" t='s' s=\"3\"><v>1</v></c>");
        assert_eq!(t.name, b"c");
        assert_eq!(t.attr("r").as_deref(), Some("B7"));
        assert_eq!(t.attr("t").as_deref(), Some("s"));
        assert_eq!(t.attr("s").as_deref(), Some("3"));
        assert_eq!(t.attr("x"), None);
        let t = first_tag("<sheet name=\"A &amp; B &#x5D0;\" r:id = 'rId2' sheetId=\"1\"/>");
        assert_eq!(t.attr("name").as_deref(), Some("A & B א"));
        assert_eq!(t.attr("r:id").as_deref(), Some("rId2"));
        assert_eq!(t.attr("id"), None);
        // A `>` and the other quote inside a value.
        let t = first_tag("<a q=\"x > 'y'\" z='\"'>after</a>");
        assert_eq!(t.attr("q").as_deref(), Some("x > 'y'"));
        assert_eq!(t.attr("z").as_deref(), Some("\""));
        assert_eq!(events("<a q=\"x > y\">after</a>"), ["<a", "after", ">a"]);
        // Malformed attributes give nothing, without a panic.
        for bad in ["<a r>", "<a r=>", "<a r=x>", "<a =\"x\">", "<a r=\"x>"] {
            let _ = Xml::new(bad.as_bytes()).count();
            if let Some(Ok(Event::Start(t))) = Xml::new(bad.as_bytes()).next() {
                assert_eq!(t.attr("r"), None, "{bad}");
            }
        }
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            events("<t>&lt;&gt;&amp;&quot;&apos;</t>"),
            ["<t", "<>&\"'", ">t"]
        );
        assert_eq!(
            events("<t>&#65;&#x42;&#X43;&#x5d0;</t>"),
            ["<t", "ABCא", ">t"]
        );
        // Invalid numeric entities: the replacement character.
        for bad in [
            "&#0;",
            "&#xD800;",
            "&#x110000;",
            "&#99999999999999999999;",
            "&#;",
            "&#x;",
            "&#12a;",
        ] {
            assert_eq!(
                events(&format!("<t>{bad}</t>")),
                ["<t", "\u{FFFD}", ">t"],
                "{bad}"
            );
        }
        // An unknown name or no `;`: the text as written.
        assert_eq!(
            events("<t>&nbsp; & x &amp</t>"),
            ["<t", "&nbsp; & x &amp", ">t"]
        );
    }

    #[test]
    fn cdata_comments_and_instructions() {
        assert_eq!(
            events("<?xml version=\"1.0\"?><a><!-- <b>x</b> --><![CDATA[<x> & y]]></a>"),
            ["<a", "<x> & y", ">a"]
        );
        assert_eq!(events("<a>x<?pi ?>y</a>"), ["<a", "x", "y", ">a"]);
        assert_eq!(events("<t>שלום עולם</t>"), ["<t", "שלום עולם", ">t"]);
        // Bytes that are not UTF-8 are replaced.
        let texts: Vec<String> = Xml::new(b"<t>a\xffb</t>")
            .filter_map(|e| match e {
                Ok(Event::Text(t)) => Some(t.into_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["a\u{FFFD}b"]);
    }

    #[test]
    fn a_document_type_declaration_is_an_error() {
        assert_eq!(
            events("<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY e \"bomb\">]><x>&e;</x>"),
            ["!"]
        );
        assert_eq!(events("<a>x</a><!doctype y>"), ["<a", "x", ">a", "!"]);
        assert_eq!(events("<!ELEMENT a ANY>"), ["!"]);
    }

    #[test]
    fn cut_input_ends_without_a_panic() {
        let full = "<?xml version=\"1.0\"?><a k=\"v&amp;w\" j='x'><!-- c --><b>t&#x5D0;u\
                    <![CDATA[d]]></b><c/></a>";
        for cut in 0..=full.len() {
            let _ = events(&full[..cut]);
            let _ = events(std::str::from_utf8(&full.as_bytes()[..cut]).unwrap_or(""));
        }
        assert_eq!(events("<a"), Vec::<String>::new());
        assert_eq!(events("<a k=\"v"), Vec::<String>::new());
        assert_eq!(events("<a>x&#x5"), ["<a", "x&#x5"]);
        assert_eq!(events("<a><![CDATA[abc"), ["<a"]);
        assert_eq!(events("<a><!-- abc"), ["<a"]);
        assert_eq!(events("<a></a"), ["<a"]);
        assert_eq!(events("<a><?pi"), ["<a"]);
    }

    /// Hostile inputs of about `n` bytes.
    fn hostile(n: usize) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = [
            "<",
            "<a",
            "<a>",
            "</a",
            "<!--",
            "<![CDATA[",
            "<?",
            "&#",
            "&",
            "&#x5d0;",
            "<a k='",
            "<a k=\"v\" ",
        ]
        .iter()
        .map(|u| u.repeat(n / u.len()).into_bytes())
        .collect();
        // Deep nesting, never closed.
        v.push("<a>".repeat(n / 3).into_bytes());
        // One very long attribute, and a long attribute list.
        v.push(format!("<a k=\"{}\">", "x".repeat(n)).into_bytes());
        v.push(format!("<a {}>", "k=\"v\" ".repeat(n / 6)).into_bytes());
        v
    }

    /// Shortest of three times of scanning `b` and asking a tag for an
    /// attribute it does not have.
    fn best_time(b: &[u8]) -> Duration {
        (0..3)
            .map(|_| {
                let t = Instant::now();
                for e in Xml::new(std::hint::black_box(b)) {
                    if let Ok(Event::Start(t)) = e {
                        std::hint::black_box(t.attr("zz"));
                    }
                }
                t.elapsed()
            })
            .min()
            .unwrap()
    }

    #[test]
    fn pathological_input_is_linear() {
        // Eight times the input must not take much more than eight times as
        // long; a quadratic scan takes 64 times as long.
        const N: usize = 1 << 15;
        let started = Instant::now();
        for (small, large) in hostile(N).iter().zip(hostile(8 * N)) {
            let (t_small, t_large) = (best_time(small), best_time(&large));
            assert!(
                t_large <= t_small * 24 + Duration::from_millis(20),
                "not linear: {t_small:?} for n, {t_large:?} for 8 n, input starts {:?}",
                String::from_utf8_lossy(&small[..16])
            );
        }
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
