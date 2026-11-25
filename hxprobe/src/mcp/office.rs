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
    take(archive, r, budget, max_entry).map(Some)
}

/// The bytes of `r`, inflated, under `max_entry` and what is left of
/// `budget`, which loses what they take.
fn take(archive: &[u8], r: &Record, budget: &mut usize, max_entry: usize) -> Result<Vec<u8>, ()> {
    let out = read(archive, r, max_entry.min(*budget))?;
    *budget -= out.len();
    Ok(out)
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

/// Most bytes of text an Office reader produces (the same number as the
/// limit on the text of the PDF child).
pub const OFFICE_MAX_TEXT: usize = 16_000_000;

/// The text a reader made, and whether it stopped at its limit.
#[derive(Debug, PartialEq, Eq)]
pub struct Cut {
    pub text: String,
    pub cut: bool,
}

/// The one way a reader adds text: every push, padding included, is checked
/// against the limit before anything is appended, so neither the text nor
/// its buffer can grow past it (the buffer at most doubles the limit).
struct TextOut {
    s: String,
    limit: usize,
    full: bool,
}

impl TextOut {
    fn new(limit: usize) -> TextOut {
        TextOut {
            s: String::new(),
            limit,
            full: false,
        }
    }

    /// Whether `added` more bytes fit; when they do not, the text is full
    /// from now on.
    fn fits(&mut self, added: Option<usize>) -> bool {
        if !self.full && added.is_some_and(|n| n <= self.limit - self.s.len()) {
            return true;
        }
        self.full = true;
        false
    }

    fn push(&mut self, c: char) {
        if self.fits(Some(c.len_utf8())) {
            self.s.push(c);
        }
    }

    fn push_str(&mut self, t: &str) {
        if self.fits(Some(t.len())) {
            self.s.push_str(t);
        }
    }

    /// `n` times `c` (padding), checked as a whole first; never builds a
    /// string of `n` characters.
    fn push_n(&mut self, c: char, n: usize) {
        if self.fits(c.len_utf8().checked_mul(n)) {
            for _ in 0..n {
                self.s.push(c);
            }
        }
    }

    fn finish(self) -> Cut {
        Cut {
            text: self.s,
            cut: self.full,
        }
    }
}

/// The text of a `.docx` archive, at most [`OFFICE_MAX_TEXT`] bytes.
// The error carries nothing on purpose: no detail of a file from mail
// reaches a result.
#[allow(clippy::result_unit_err)]
pub fn docx_text(archive: &[u8]) -> Result<Cut, ()> {
    docx_text_limited(archive, OFFICE_MAX_TEXT)
}

/// Feeds the events of `xml` to `f` until `out` is full; a scanner error is
/// an error of the reader.
fn scan<'a>(
    xml: &'a [u8],
    out: &mut TextOut,
    mut f: impl FnMut(Event<'a>, &mut TextOut),
) -> Result<(), ()> {
    for e in Xml::new(xml) {
        f(e?, out);
        if out.full {
            break;
        }
    }
    Ok(())
}

/// What the end of a paragraph or a cell of a Word table has left to write
/// before the next text of the same row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Nothing,
    Break,
    Tab,
}

/// [`docx_text`] with a given limit. Text of `w:t`; `w:tab` a tab (not the
/// tab stops of `w:tabs`); `w:br` and `w:cr` a line break; the end of a
/// `w:p` a line break. In a table, cells of a row are separated by tabs and
/// a row ends with a line break; paragraphs inside one cell are separated
/// by line breaks. Headers, footers, notes and comments are not read.
fn docx_text_limited(archive: &[u8], limit: usize) -> Result<Cut, ()> {
    let mut budget = MAX_INFLATED;
    let doc = entry(archive, "word/document.xml", &mut budget)?.ok_or(())?;
    let mut out = TextOut::new(limit);
    let (mut in_t, mut tab_stops, mut cells) = (false, 0usize, 0usize);
    let mut pending = Pending::Nothing;
    // Writes what a cell left pending, before new text.
    let flush = |out: &mut TextOut, pending: &mut Pending| {
        match *pending {
            Pending::Nothing => {}
            Pending::Break => out.push('\n'),
            Pending::Tab => out.push('\t'),
        }
        *pending = Pending::Nothing;
    };
    scan(&doc, &mut out, |e, out| match e {
        Event::Start(t) => match t.name {
            b"w:t" => in_t = true,
            b"w:tabs" => tab_stops += 1,
            b"w:tab" if tab_stops == 0 => {
                flush(out, &mut pending);
                out.push('\t');
            }
            b"w:br" | b"w:cr" => {
                flush(out, &mut pending);
                out.push('\n');
            }
            b"w:tc" => cells += 1,
            _ => {}
        },
        Event::End(name) => match name {
            b"w:t" => in_t = false,
            b"w:tabs" => tab_stops = tab_stops.saturating_sub(1),
            b"w:p" if cells > 0 => {
                if pending == Pending::Nothing {
                    pending = Pending::Break;
                }
            }
            b"w:p" => out.push('\n'),
            b"w:tc" => {
                cells = cells.saturating_sub(1);
                // The tab of an empty cell before this one.
                if pending == Pending::Tab {
                    out.push('\t');
                }
                pending = Pending::Tab;
            }
            b"w:tr" => {
                pending = Pending::Nothing;
                out.push('\n');
            }
            _ => {}
        },
        Event::Text(t) if in_t => {
            flush(out, &mut pending);
            out.push_str(&t);
        }
        Event::Text(_) => {}
    })?;
    Ok(out.finish())
}

/// The text of a `.pptx` archive, at most [`OFFICE_MAX_TEXT`] bytes.
// The error carries nothing on purpose: no detail of a file from mail
// reaches a result.
#[allow(clippy::result_unit_err)]
pub fn pptx_text(archive: &[u8]) -> Result<Cut, ()> {
    pptx_text_limited(archive, OFFICE_MAX_TEXT)
}

/// The number `N` of an entry named `<prefix><N>.xml`.
fn numbered(name: &[u8], prefix: &str) -> Option<u32> {
    let digits = name
        .strip_prefix(prefix.as_bytes())?
        .strip_suffix(b".xml")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// [`pptx_text`] with a given limit. The entries `ppt/slides/slide<N>.xml`
/// in number order, each with a line `# Slide <N>`, then the text of `a:t`,
/// a line break at the end of `a:p` and for `a:br`. Notes pages and the
/// slide order of the presentation part are not read.
fn pptx_text_limited(archive: &[u8], limit: usize) -> Result<Cut, ()> {
    let records = directory(archive)?;
    let mut slides: Vec<(u32, &Record)> = records
        .iter()
        .filter_map(|r| Some((numbered(r.name, "ppt/slides/slide")?, r)))
        .collect();
    slides.sort_by_key(|(n, _)| *n);
    slides.dedup_by_key(|(n, _)| *n);
    let mut budget = MAX_INFLATED;
    let mut out = TextOut::new(limit);
    for (n, r) in slides {
        out.push_str(&format!("# Slide {n}\n"));
        if out.full {
            break;
        }
        let xml = take(archive, r, &mut budget, MAX_ENTRY_BYTES)?;
        let mut in_t = false;
        scan(&xml, &mut out, |e, out| match e {
            Event::Start(t) if t.name == b"a:t" => in_t = true,
            Event::Start(t) if t.name == b"a:br" => out.push('\n'),
            Event::End(b"a:t") => in_t = false,
            Event::End(b"a:p") => out.push('\n'),
            Event::Text(t) if in_t => out.push_str(&t),
            _ => {}
        })?;
        if out.full {
            break;
        }
    }
    Ok(out.finish())
}

/// Most shared strings of a workbook kept; later ones read as empty cells.
const MAX_SHARED: usize = 1_000_000;
/// Largest column of a workbook (`XFD`).
const MAX_COLUMN: u32 = 16_384;
/// Largest row of a workbook.
const MAX_ROW: u64 = 1_048_576;

/// The shared strings of a workbook: one `String` and the byte range of
/// each entry in it, so an empty entry costs 8 bytes and no allocation.
struct Shared {
    pool: String,
    ranges: Vec<(u32, u32)>,
}

impl Shared {
    /// Entry `i`; empty for one that was not kept or does not exist.
    fn get(&self, i: usize) -> &str {
        self.ranges
            .get(i)
            .and_then(|&(a, b)| self.pool.get(a as usize..b as usize))
            .unwrap_or_default()
    }
}

/// The name of an element without its prefix (`x:row` is `row`).
fn local(name: &[u8]) -> &[u8] {
    name.iter()
        .position(|c| *c == b':')
        .map_or(name, |i| &name[i + 1..])
}

/// The shared strings of `xml` (`xl/sharedStrings.xml`): the text of the `t`
/// elements of each `si`, phonetic runs (`rPh`) left out. At most
/// [`MAX_SHARED`] entries and [`MAX_ENTRY_BYTES`] bytes of text are kept;
/// what comes after reads as empty.
fn shared_strings(xml: &[u8]) -> Result<Shared, ()> {
    let mut s = Shared {
        pool: String::new(),
        ranges: Vec::new(),
    };
    let (mut start, mut in_si, mut in_t, mut phonetic) = (0usize, false, false, 0usize);
    for e in Xml::new(xml) {
        match e? {
            Event::Start(t) => match local(t.name) {
                b"si" => {
                    in_si = true;
                    start = s.pool.len();
                }
                b"t" => in_t = true,
                b"rPh" => phonetic += 1,
                _ => {}
            },
            Event::End(name) => match local(name) {
                b"si" => {
                    in_si = false;
                    // Both ends fit: the pool never passes MAX_ENTRY_BYTES.
                    s.ranges.push((start as u32, s.pool.len() as u32));
                    if s.ranges.len() >= MAX_SHARED {
                        break;
                    }
                }
                b"t" => in_t = false,
                b"rPh" => phonetic = phonetic.saturating_sub(1),
                _ => {}
            },
            Event::Text(t) if in_si && in_t && phonetic == 0 => {
                if s.pool.len() + t.len() <= MAX_ENTRY_BYTES {
                    s.pool.push_str(&t);
                }
            }
            Event::Text(_) => {}
        }
    }
    Ok(s)
}

/// The column of a cell reference (`C7` is 3): at most three letters, at
/// most [`MAX_COLUMN`]; `None` for any other reference.
fn column(reference: &str) -> Option<u32> {
    let letters = reference
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .take(4)
        .collect::<Vec<u8>>();
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }
    let n = letters.iter().fold(0u32, |n, c| {
        n * 26 + u32::from(c.to_ascii_uppercase() - b'A' + 1)
    });
    (n <= MAX_COLUMN).then_some(n)
}

/// A row number, capped at [`MAX_ROW`]; `None` for one that is not a
/// number.
fn row_number(r: &str) -> Option<u64> {
    if r.is_empty() || !r.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(r.bytes().fold(0u64, |n, c| {
        n.saturating_mul(10)
            .saturating_add(u64::from(c - b'0'))
            .min(MAX_ROW)
    }))
}

/// How the value of a cell is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellType {
    /// An index into the shared strings.
    Shared,
    /// `TRUE` or `FALSE`.
    Bool,
    /// The text of `is`.
    Inline,
    /// The stored value as written.
    Plain,
}

/// The rows of one worksheet into `out`: one line per row, cells at their
/// column, separated by tabs; empty lines for a gap in row numbers.
fn sheet_rows(xml: &[u8], shared: &Shared, out: &mut TextOut) -> Result<(), ()> {
    let mut row = 0u64;
    // The column of the last cell seen and of the last cell written.
    let (mut seen, mut written) = (0u32, 0u32);
    // The open cell: its column (`None`: skipped) and type.
    let mut cell: Option<(Option<u32>, CellType)> = None;
    let (mut in_v, mut in_is, mut in_t, mut phonetic) = (false, false, false, 0usize);
    let mut value = String::new();
    for e in Xml::new(xml) {
        match e? {
            Event::Start(t) => match local(t.name) {
                b"row" => {
                    let next = (row + 1).min(MAX_ROW);
                    let n = t.attr("r").and_then(|r| row_number(&r)).unwrap_or(next);
                    if n > row + 1 {
                        // At most MAX_ROW, so it fits a usize.
                        out.push_n('\n', (n - row - 1) as usize);
                    }
                    row = n;
                    (seen, written) = (0, 0);
                }
                b"c" => {
                    let col = match t.attr("r") {
                        Some(r) => column(&r),
                        None => Some(seen + 1),
                    }
                    .map(|c| c.max(seen + 1))
                    .filter(|c| *c <= MAX_COLUMN);
                    let kind = match t.attr("t").as_deref() {
                        Some("s") => CellType::Shared,
                        Some("b") => CellType::Bool,
                        Some("inlineStr") => CellType::Inline,
                        _ => CellType::Plain,
                    };
                    cell = Some((col, kind));
                    value.clear();
                }
                b"v" => in_v = true,
                b"is" => in_is = true,
                b"t" => in_t = true,
                b"rPh" => phonetic += 1,
                _ => {}
            },
            Event::End(name) => match local(name) {
                b"row" => out.push('\n'),
                b"c" => {
                    if let Some((Some(col), kind)) = cell.take() {
                        let text = match kind {
                            CellType::Shared => {
                                value.trim().parse().map_or("", |i: usize| shared.get(i))
                            }
                            CellType::Bool => match value.trim() {
                                "1" => "TRUE",
                                "0" => "FALSE",
                                v => v,
                            },
                            CellType::Inline | CellType::Plain => &value,
                        };
                        if !text.is_empty() {
                            out.push_n('\t', (col - written.max(1)) as usize);
                            out.push_str(text);
                            written = col;
                        }
                        seen = col;
                    }
                }
                b"v" => in_v = false,
                b"is" => in_is = false,
                b"t" => in_t = false,
                b"rPh" => phonetic = phonetic.saturating_sub(1),
                _ => {}
            },
            Event::Text(t) => {
                let wanted = match cell {
                    Some((Some(_), CellType::Inline)) => in_is && in_t && phonetic == 0,
                    Some((Some(_), _)) => in_v,
                    _ => false,
                };
                if wanted {
                    // A value that cannot fit in what is left of the text
                    // ends it here, before it is held in memory.
                    if value.len() + t.len() > out.limit - out.s.len() {
                        out.fits(None);
                    } else {
                        value.push_str(&t);
                    }
                }
            }
        }
        if out.full {
            break;
        }
    }
    Ok(())
}

/// The part name of a relation target of the workbook part.
fn part_name(target: &str) -> String {
    match target.strip_prefix('/') {
        Some(absolute) => absolute.to_string(),
        None => format!("xl/{target}"),
    }
}

/// The text of a `.xlsx` archive, at most [`OFFICE_MAX_TEXT`] bytes.
// The error carries nothing on purpose: no detail of a file from mail
// reaches a result.
#[allow(clippy::result_unit_err)]
pub fn xlsx_text(archive: &[u8]) -> Result<Cut, ()> {
    xlsx_text_limited(archive, OFFICE_MAX_TEXT)
}

/// [`xlsx_text`] with a given limit. Sheet names and order from
/// `xl/workbook.xml` and its relations; without a usable relation the
/// entries `xl/worksheets/sheet<N>.xml` in number order, named `Sheet <N>`.
/// Per sheet a line `# <name>`, then its rows (see [`sheet_rows`]).
fn xlsx_text_limited(archive: &[u8], limit: usize) -> Result<Cut, ()> {
    use std::collections::HashMap;
    let records = directory(archive)?;
    let by_name: HashMap<&[u8], &Record> = records.iter().map(|r| (r.name, r)).collect();
    let mut budget = MAX_INFLATED;
    let part = |name: &str, budget: &mut usize| match by_name.get(name.as_bytes()) {
        Some(r) => take(archive, r, budget, MAX_ENTRY_BYTES).map(Some),
        None => Ok(None),
    };

    // Relation id to part name, then the sheets in workbook order.
    let mut targets: HashMap<String, String> = HashMap::new();
    if let Some(rels) = part("xl/_rels/workbook.xml.rels", &mut budget)? {
        for e in Xml::new(&rels) {
            if let Event::Start(t) = e? {
                if local(t.name) == b"Relationship" && targets.len() < MAX_ENTRIES {
                    if let (Some(id), Some(target)) = (t.attr("Id"), t.attr("Target")) {
                        targets.insert(id.into_owned(), part_name(&target));
                    }
                }
            }
        }
    }
    let mut sheets: Vec<(String, &Record)> = Vec::new();
    if let Some(workbook) = part("xl/workbook.xml", &mut budget)? {
        for e in Xml::new(&workbook) {
            if let Event::Start(t) = e? {
                if local(t.name) != b"sheet" || sheets.len() >= MAX_ENTRIES {
                    continue;
                }
                let target = t.attr("r:id").and_then(|id| targets.get(id.as_ref()));
                if let Some(r) = target.and_then(|p| by_name.get(p.as_bytes())) {
                    let name = t.attr("name").unwrap_or_default().into_owned();
                    sheets.push((name, r));
                }
            }
        }
    }
    if sheets.is_empty() {
        let mut numbered: Vec<(u32, &Record)> = records
            .iter()
            .filter_map(|r| Some((numbered(r.name, "xl/worksheets/sheet")?, r)))
            .collect();
        numbered.sort_by_key(|(n, _)| *n);
        numbered.dedup_by_key(|(n, _)| *n);
        sheets = numbered
            .into_iter()
            .map(|(n, r)| (format!("Sheet {n}"), r))
            .collect();
    }

    let shared = match part("xl/sharedStrings.xml", &mut budget)? {
        Some(xml) => shared_strings(&xml)?,
        None => Shared {
            pool: String::new(),
            ranges: Vec::new(),
        },
    };
    let mut out = TextOut::new(limit);
    for (name, r) in sheets {
        out.push_str("# ");
        out.push_str(&name);
        out.push('\n');
        if out.full {
            break;
        }
        let xml = take(archive, r, &mut budget, MAX_ENTRY_BYTES)?;
        sheet_rows(&xml, &shared, &mut out)?;
        if out.full {
            break;
        }
    }
    Ok(out.finish())
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

#[cfg(test)]
mod reader_tests {
    use super::testzip::zip;
    use super::*;

    #[test]
    fn text_out_stops_at_its_limit() {
        let mut t = TextOut::new(10);
        t.push_str("abc");
        t.push('d');
        t.push_n('-', 3);
        t.push_str("");
        assert!(!t.full);
        // Would pass the limit: nothing of it is appended.
        t.push_str("wxyz");
        assert!(t.full);
        // Every later push is a no-op, even one that would fit.
        t.push('a');
        t.push_str("b");
        t.push_n('c', 1);
        assert_eq!(
            t.finish(),
            Cut {
                text: "abcd---".into(),
                cut: true
            }
        );
        // Up to the limit exactly is not full; a character is whole or not
        // at all.
        let mut t = TextOut::new(4);
        t.push_str("אב");
        assert!(!t.full);
        t.push('ג');
        assert!(t.full);
        assert_eq!(t.finish().text, "אב");
        let mut t = TextOut::new(5);
        t.push_n('א', 2);
        t.push_n('א', 1);
        assert_eq!(
            t.finish(),
            Cut {
                text: "אא".into(),
                cut: true
            }
        );
    }

    #[test]
    fn text_out_padding_never_builds_its_string() {
        let started = std::time::Instant::now();
        let mut t = TextOut::new(1_000);
        t.push_str(&"x".repeat(500));
        t.push_n(' ', usize::MAX / 2);
        assert!(t.full);
        t.push_n('\t', usize::MAX);
        assert!(t.s.len() <= 1_000);
        assert!(t.s.capacity() <= 2_000, "capacity {}", t.s.capacity());
        let mut t = TextOut::new(1_000);
        t.push_n('\u{10FFFF}', usize::MAX / 2);
        assert!(t.full);
        assert!(t.s.capacity() <= 2_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // Filling up to the limit keeps the capacity within twice of it.
        let mut t = TextOut::new(1_000);
        for _ in 0..2_000 {
            t.push('y');
        }
        assert_eq!(t.s.len(), 1_000);
        assert!(t.s.capacity() <= 2_000);
    }

    /// A `.docx` archive whose body is `body`.
    fn docx(body: &str) -> Vec<u8> {
        let doc = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
             <w:body>{body}</w:body></w:document>"
        );
        zip(&[
            ("[Content_Types].xml", b"<Types/>", true),
            ("word/document.xml", doc.as_bytes(), true),
        ])
    }

    #[test]
    fn docx_paragraphs_runs_tabs_and_breaks() {
        let a = docx(
            "<w:p><w:pPr><w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs></w:pPr>\n\
             <w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space=\"preserve\"> wor</w:t></w:r>\
             <w:r><w:rPr><w:b/></w:rPr><w:t>ld</w:t></w:r></w:p>\n\
             <w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>c</w:t><w:cr/>\
             <w:t>d &amp; e</w:t></w:r></w:p>\n\
             <w:p/><w:p><w:r><w:instrText>PAGE</w:instrText><w:t>שלום</w:t></w:r></w:p>",
        );
        assert_eq!(
            docx_text(&a),
            Ok(Cut {
                text: "Hello world\na\tb\nc\nd & e\n\nשלום\n".into(),
                cut: false
            })
        );
    }

    #[test]
    fn docx_tables_give_tabs_between_cells_and_a_line_per_row() {
        let cell = |t: &str| format!("<w:tc><w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>");
        let a = docx(&format!(
            "<w:p><w:r><w:t>before</w:t></w:r></w:p>\
             <w:tbl><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>\
             <w:p><w:r><w:t>after</w:t></w:r></w:p>",
            cell("A1"),
            cell("B1"),
            cell("A2"),
            cell("B2")
        ));
        assert_eq!(
            docx_text(&a).unwrap().text,
            "before\nA1\tB1\nA2\tB2\nafter\n"
        );
        // An empty cell keeps its place; two paragraphs in a cell are two
        // lines of it.
        let a = docx(
            "<w:tbl><w:tr><w:tc><w:p/></w:tc>\
             <w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p><w:p><w:r><w:t>y</w:t></w:r></w:p></w:tc>\
             </w:tr></w:tbl>",
        );
        assert_eq!(docx_text(&a).unwrap().text, "\tx\ny\n");
    }

    #[test]
    fn docx_failures() {
        // No `word/document.xml`.
        let a = zip(&[("xl/workbook.xml", b"<workbook/>", true)]);
        assert_eq!(docx_text(&a), Err(()));
        assert_eq!(docx_text(b"not an archive"), Err(()));
        // A document type declaration.
        let a = zip(&[(
            "word/document.xml",
            b"<!DOCTYPE d [<!ENTITY e \"x\">]><w:document/>",
            true,
        )]);
        assert_eq!(docx_text(&a), Err(()));
        // No text at all is not a failure.
        assert_eq!(
            docx_text(&docx("<w:p/>")),
            Ok(Cut {
                text: "\n".into(),
                cut: false
            })
        );
    }

    #[test]
    fn docx_text_stops_at_its_limit() {
        let body: String = (0..1_000)
            .map(|i| format!("<w:p><w:r><w:t>paragraph {i}</w:t></w:r></w:p>"))
            .collect();
        let a = docx(&body);
        let c = docx_text_limited(&a, 100).unwrap();
        assert!(c.cut);
        assert!(c.text.len() <= 100);
        assert!(c.text.starts_with("paragraph 0\nparagraph 1\n"));
        let all = docx_text(&a).unwrap();
        assert!(!all.cut);
        assert!(all.text.ends_with("paragraph 999\n"));
    }

    /// A `.pptx` archive with the given slides `(number, paragraphs)`, in
    /// the order given.
    fn pptx(slides: &[(u32, &[&str])]) -> Vec<u8> {
        let xml: Vec<(String, String)> = slides
            .iter()
            .map(|(n, paras)| {
                let ps: String = paras
                    .iter()
                    .map(|p| format!("<a:p><a:r><a:t>{p}</a:t></a:r></a:p>"))
                    .collect();
                (
                    format!("ppt/slides/slide{n}.xml"),
                    format!(
                        "<p:sld><p:cSld><p:spTree><p:sp><p:txBody>{ps}</p:txBody></p:sp>\
                         </p:spTree></p:cSld></p:sld>"
                    ),
                )
            })
            .collect();
        let mut entries: Vec<(&str, &[u8], bool)> = vec![
            ("ppt/presentation.xml", b"<p:presentation/>", true),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                b"<Relationships/>",
                true,
            ),
            (
                "ppt/slideLayouts/slideLayout1.xml",
                b"<a:t>layout</a:t>",
                true,
            ),
            ("ppt/notesSlides/notesSlide1.xml", b"<a:t>notes</a:t>", true),
        ];
        entries.extend(xml.iter().map(|(n, x)| (n.as_str(), x.as_bytes(), true)));
        zip(&entries)
    }

    #[test]
    fn pptx_slides_come_in_number_order() {
        let a = pptx(&[
            (3, &["Three"]),
            (1, &["Title one", "Body & more"]),
            (2, &[]),
        ]);
        assert_eq!(
            pptx_text(&a),
            Ok(Cut {
                text: "# Slide 1\nTitle one\nBody & more\n# Slide 2\n# Slide 3\nThree\n".into(),
                cut: false
            })
        );
        let a = pptx(&[(10, &["ten"]), (9, &["nine"]), (1, &["one"])]);
        assert_eq!(
            pptx_text(&a).unwrap().text,
            "# Slide 1\none\n# Slide 9\nnine\n# Slide 10\nten\n"
        );
    }

    #[test]
    fn pptx_line_breaks_and_failures() {
        let a = zip(&[
            ("ppt/presentation.xml", b"<p:presentation/>", true),
            (
                "ppt/slides/slide1.xml",
                b"<p:sld><a:p><a:r><a:t>a</a:t></a:r><a:br/><a:r><a:t>b</a:t></a:r></a:p></p:sld>",
                false,
            ),
        ]);
        assert_eq!(pptx_text(&a).unwrap().text, "# Slide 1\na\nb\n");
        assert_eq!(pptx_text(b"not an archive"), Err(()));
        let a = zip(&[("ppt/slides/slide1.xml", b"<!DOCTYPE x><p:sld/>", true)]);
        assert_eq!(pptx_text(&a), Err(()));
        // A presentation without slides has no text.
        let a = zip(&[("ppt/presentation.xml", b"<p:presentation/>", true)]);
        assert_eq!(
            pptx_text(&a),
            Ok(Cut {
                text: String::new(),
                cut: false
            })
        );
    }

    #[test]
    fn pptx_text_stops_at_its_limit() {
        let paras: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = paras.iter().map(String::as_str).collect();
        let slides: Vec<(u32, &[&str])> = (1..=50).map(|n| (n, &refs[..])).collect();
        let a = pptx(&slides);
        let c = pptx_text_limited(&a, 1_000).unwrap();
        assert!(c.cut);
        assert!(c.text.len() <= 1_000);
        assert!(c.text.starts_with("# Slide 1\nline 0\n"));
        assert!(!pptx_text(&a).unwrap().cut);
    }
}

#[cfg(test)]
mod xlsx_tests {
    use super::testzip::zip;
    use super::*;
    use std::time::{Duration, Instant};

    const WORKBOOK: &str = "<workbook xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>\
         <sheet name=\"Summary &amp; totals\" sheetId=\"2\" r:id=\"rId7\"/>\
         <sheet name=\"Data\" sheetId=\"1\" r:id=\"rId3\"/></sheets></workbook>";
    const RELS: &str = "<Relationships>\
         <Relationship Id=\"rId3\" Type=\"worksheet\" Target=\"worksheets/sheet1.xml\"/>\
         <Relationship Id=\"rId9\" Type=\"styles\" Target=\"styles.xml\"/>\
         <Relationship Id=\"rId7\" Type=\"worksheet\" Target=\"/xl/worksheets/sheet2.xml\"/>\
         </Relationships>";

    /// A worksheet whose `sheetData` holds `rows`.
    fn sheet(rows: &str) -> String {
        format!("<worksheet><sheetData>{rows}</sheetData></worksheet>")
    }

    /// A workbook of the given sheet entries, with or without the workbook
    /// relations, and with shared strings when given.
    fn xlsx(rels: bool, shared: Option<&str>, sheets: &[(&str, String)]) -> Vec<u8> {
        let mut e: Vec<(&str, &[u8], bool)> = vec![("xl/workbook.xml", WORKBOOK.as_bytes(), true)];
        if rels {
            e.push(("xl/_rels/workbook.xml.rels", RELS.as_bytes(), true));
        }
        if let Some(s) = shared {
            e.push(("xl/sharedStrings.xml", s.as_bytes(), true));
        }
        e.extend(sheets.iter().map(|(n, x)| (*n, x.as_bytes(), true)));
        zip(&e)
    }

    #[test]
    fn sheets_in_workbook_order_with_their_cells() {
        let shared =
            "<sst count=\"4\"><si><t>Name</t></si><si><t>alpha</t><rPh><t>phonetic</t></rPh></si>\
             <si><r><t>beta </t></r><r><rPr><b/></rPr><t>&amp; co</t></r></si><si/></sst>";
        let summary = sheet(
            "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"inlineStr\"><is><t>Count</t></is></c>\
             <c r=\"D1\" t=\"str\"><v>OK</v></c></row>\
             <row r=\"2\"><c r=\"A2\" t=\"s\"><v>1</v></c><c r=\"B2\"><v>3.5</v></c>\
             <c r=\"D2\" t=\"b\"><v>1</v></c><c r=\"E2\" t=\"s\"><v>3</v></c></row>\
             <row r=\"4\"><c r=\"A4\" t=\"s\"><v>2</v></c><c r=\"B4\"><f>SUM(B1:B2)</f><v>42</v></c>\
             <c r=\"D4\" t=\"b\"><v>0</v></c><c r=\"C4\" s=\"1\"/></row>",
        );
        let data = sheet("<row r=\"1\"><c r=\"A1\"><v>45291</v></c></row>");
        let a = xlsx(
            true,
            Some(shared),
            &[
                ("xl/worksheets/sheet1.xml", data),
                ("xl/worksheets/sheet2.xml", summary),
            ],
        );
        assert_eq!(
            xlsx_text(&a),
            Ok(Cut {
                text: "# Summary & totals\nName\tCount\t\tOK\nalpha\t3.5\t\tTRUE\n\n\
                       beta & co\t42\t\tFALSE\n# Data\n45291\n"
                    .into(),
                cut: false
            })
        );
    }

    #[test]
    fn without_relations_sheets_come_in_number_order() {
        let one = |v: &str| sheet(&format!("<row><c><v>{v}</v></c></row>"));
        let a = xlsx(
            false,
            None,
            &[
                ("xl/worksheets/sheet10.xml", one("ten")),
                ("xl/worksheets/sheet2.xml", one("two")),
                ("xl/worksheets/sheet1.xml", one("one")),
                ("xl/worksheets/_rels/sheet1.xml.rels", "<x/>".into()),
            ],
        );
        assert_eq!(
            xlsx_text(&a).unwrap().text,
            "# Sheet 1\none\n# Sheet 2\ntwo\n# Sheet 10\nten\n"
        );
    }

    #[test]
    fn a_shared_string_past_the_end_is_an_empty_cell() {
        let a = xlsx(
            true,
            Some("<sst><si><t>only</t></si></sst>"),
            &[(
                "xl/worksheets/sheet1.xml",
                sheet(
                    "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>99</v></c><c r=\"B1\" t=\"s\"><v>x</v></c>\
                     <c r=\"C1\" t=\"s\"><v>-1</v></c><c r=\"D1\" t=\"s\"><v>99999999999999999999999</v></c>\
                     <c r=\"E1\" t=\"s\"><v>0</v></c></row>",
                ),
            )],
        );
        assert_eq!(xlsx_text(&a).unwrap().text, "# Data\n\t\t\t\tonly\n");
        // No shared strings at all: the same.
        let a = xlsx(
            true,
            None,
            &[(
                "xl/worksheets/sheet1.xml",
                sheet("<row><c t=\"s\"><v>0</v></c></row>"),
            )],
        );
        assert_eq!(xlsx_text(&a).unwrap().text, "# Data\n\n");
    }

    #[test]
    fn three_million_empty_shared_strings_are_ranges_only() {
        let xml = format!(
            "<sst><si><t>first</t></si>{}</sst>",
            "<si/>".repeat(3_000_000)
        );
        let s = shared_strings(xml.as_bytes()).unwrap();
        assert_eq!(s.ranges.len(), MAX_SHARED);
        assert!(s.ranges.capacity() <= 2 * MAX_SHARED);
        assert!(
            s.pool.capacity() < 1_000,
            "pool capacity {}",
            s.pool.capacity()
        );
        assert_eq!(s.get(0), "first");
        assert_eq!(s.get(1), "");
        assert_eq!(s.get(2_000_000), "");
        assert_eq!(s.get(usize::MAX), "");
        // Through a workbook: entry 2,000,000 is an empty cell.
        let a = xlsx(
            true,
            Some(&xml),
            &[(
                "xl/worksheets/sheet1.xml",
                sheet("<row><c r=\"A1\" t=\"s\"><v>2000000</v></c><c r=\"B1\" t=\"s\"><v>0</v></c></row>"),
            )],
        );
        assert_eq!(xlsx_text(&a).unwrap().text, "# Data\n\tfirst\n");
    }

    /// The text of a workbook with one sheet of `rows`.
    fn one_sheet(rows: &str) -> String {
        xlsx_text(&xlsx(
            true,
            None,
            &[("xl/worksheets/sheet1.xml", sheet(rows))],
        ))
        .unwrap()
        .text
    }

    #[test]
    fn column_bounds() {
        assert_eq!(
            one_sheet("<row r=\"1\"><c r=\"XFD1\" t=\"str\"><v>last</v></c></row>"),
            format!("# Data\n{}last\n", "\t".repeat(16_383))
        );
        assert_eq!(
            one_sheet(
                "<row r=\"1\"><c r=\"XFE1\"><v>1</v></c><c r=\"AAAAAAAAAAAAA7\"><v>2</v></c>\
                 <c r=\"ZZZZ1\"><v>3</v></c><c r=\"7\"><v>4</v></c><c r=\"B1\"><v>5</v></c></row>"
            ),
            "# Data\n\t5\n"
        );
        // Lower case letters, and cells out of order: each later cell takes
        // the next column at least.
        assert_eq!(
            one_sheet("<row><c r=\"c1\"><v>c</v></c><c r=\"A1\"><v>a</v></c><c><v>n</v></c></row>"),
            "# Data\n\t\tc\ta\tn\n"
        );
    }

    #[test]
    fn row_bounds() {
        let t = one_sheet(
            "<row r=\"1\"><c><v>a</v></c></row><row r=\"4294967295\"><c><v>b</v></c></row>\
             <row r=\"99999999999999999999999999\"><c><v>c</v></c></row>",
        );
        let empty = 1_048_576 - 2;
        assert_eq!(t, format!("# Data\na\n{}b\nc\n", "\n".repeat(empty)));
        // A row not above the previous one gives no gap; a row without a
        // number follows the previous one.
        assert_eq!(
            one_sheet(
                "<row r=\"5\"><c><v>5</v></c></row><row r=\"3\"><c><v>3</v></c></row>\
                 <row><c><v>n</v></c></row><row r=\"x\"/><row r=\"6\"/>"
            ),
            "# Data\n\n\n\n\n5\n3\nn\n\n\n"
        );
    }

    /// Checks of a hostile workbook read with a limit of 1,000,000 bytes.
    fn bounded(a: &[u8]) {
        let started = Instant::now();
        let c = xlsx_text_limited(a, 1_000_000).unwrap();
        assert!(c.cut);
        assert!(c.text.len() <= 1_000_000);
        assert!(
            c.text.capacity() <= 2_000_000,
            "capacity {}",
            c.text.capacity()
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn many_rows_at_the_last_column_stop_at_the_limit() {
        let rows: String = (1..=100_000)
            .map(|r| format!("<row r=\"{r}\"><c r=\"XFD{r}\"><v>1</v></c></row>"))
            .collect();
        bounded(&xlsx(
            true,
            None,
            &[("xl/worksheets/sheet1.xml", sheet(&rows))],
        ));
    }

    #[test]
    fn the_last_row_in_many_sheets_stops_at_the_limit() {
        let names: Vec<String> = (1..=20)
            .map(|n| format!("xl/worksheets/sheet{n}.xml"))
            .collect();
        let s = sheet("<row r=\"1048576\"><c><v>1</v></c></row>");
        let sheets: Vec<(&str, String)> = names.iter().map(|n| (n.as_str(), s.clone())).collect();
        bounded(&xlsx(false, None, &sheets));
    }

    #[test]
    fn many_references_to_one_large_shared_string_stop_at_the_limit() {
        let shared = format!("<sst><si><t>{}</t></si></sst>", "s".repeat(100_000));
        let rows: String = (1..=100_000)
            .map(|r| format!("<row r=\"{r}\"><c r=\"A{r}\" t=\"s\"><v>0</v></c></row>"))
            .collect();
        bounded(&xlsx(
            true,
            Some(&shared),
            &[("xl/worksheets/sheet1.xml", sheet(&rows))],
        ));
    }

    #[test]
    fn failures() {
        assert_eq!(xlsx_text(b"not an archive"), Err(()));
        let a = xlsx(
            true,
            None,
            &[(
                "xl/worksheets/sheet1.xml",
                "<!DOCTYPE x><worksheet/>".into(),
            )],
        );
        assert_eq!(xlsx_text(&a), Err(()));
        // A workbook without sheets has no text.
        assert_eq!(
            xlsx_text(&xlsx(false, None, &[])),
            Ok(Cut {
                text: String::new(),
                cut: false
            })
        );
    }
}
