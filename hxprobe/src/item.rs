//! Object items: the typed, fixed-layout structs stored inside page values.
//!
//! ```text
//! u32 size                       prefix (not part of the item)
//! item (offsets relative to the 05 00 byte):
//!   +0  u16 5 | +2 u16 fixed_len | +4 u32 size (== prefix) | +8 u16 0 | +10 u16 object_type
//!   +0x68 u32 A_len              size of heap A (inside the 0x78-byte header)
//!   item[0..fixed_len]           fixed part (fixed_len includes the header)
//!   item[fixed_len..size]        heap = heap A (A_len bytes), then heap B
//!
//! variable field descriptor inside the fixed part:
//!   u32 offset | u32 v           len = v & 0x7fffffff, 0 = not set;
//!                                bit 31 clear: offset into heap A,
//!                                bit 31 set:   offset into heap B
//! ```
//!
//! Timestamps are `u64` .NET ticks (100 ns since 0001-01-01) at fixed offsets.
//! Items are found by an envelope scan ([`scan`]), which is self-checking
//! (the size appears twice) and finds nested items as well.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::le::{u16_at, u32_at, u64_at};

/// Ticks at 1970-01-01 (the Unix epoch).
pub const UNIX_EPOCH_TICKS: u64 = 621_355_968_000_000_000;
/// Ticks per second.
pub const TICKS_PER_SECOND: u64 = 10_000_000;
/// Unix seconds at 2000-01-01, the lower bound of [`tick_window`].
const UNIX_2000: u64 = 946_684_800;
/// Look-ahead of [`tick_window`] past the current time: 5 x 365 days.
const AHEAD_SECONDS: u64 = 5 * 365 * 86_400;
/// Top bit of a descriptor's length word: set = data in heap B.
const DESC_FLAG: u32 = 0x8000_0000;
/// Item offset of `A_len`, the size of heap A.
const HEAP_A_LEN_AT: usize = 0x68;
/// First item offset searched for descriptors.
const DESC_START: usize = 16;
/// Work budget of [`scan_budgeted`]: the accepted items of one value may total
/// at most this many times the value length. Measured on the reference store
/// (353,931 items in 73,699 values): items never nest or overlap, the largest
/// total is 0.9998 x the value length, so 4 leaves room for four full levels
/// of nesting.
pub const ITEM_BUDGET_FACTOR: usize = 4;
/// Work budget of [`FieldBudget`]: the variable fields taken from one item may
/// total at most 4 x its heap length. On the reference store no field is
/// refused (`skipped_fields` is 0).
pub const FIELD_BUDGET_FACTOR: usize = 4;

/// One object item found in a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item<'a> {
    pub type_id: u16,
    pub fixed_len: usize,
    /// The whole item, `size` bytes starting at its `05 00`.
    pub bytes: &'a [u8],
    /// Offset of the item start within the scanned value.
    pub offset: usize,
}

/// A variable field: the descriptor's item offset and the heap bytes it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarField<'a> {
    pub at: usize,
    pub data: &'a [u8],
}

/// Result of [`scan_budgeted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan<'a> {
    pub items: Vec<Item<'a>>,
    /// Valid envelopes left out because the value's work budget was spent.
    pub skipped: usize,
}

/// Limits the variable-field bytes taken from one item to
/// [`FIELD_BUDGET_FACTOR`] times its heap length, so descriptors that all name
/// the same heap bytes cannot multiply the output.
#[derive(Debug)]
pub struct FieldBudget {
    left: usize,
    /// Fields refused so far.
    pub skipped: usize,
}

impl FieldBudget {
    pub fn new(it: &Item) -> FieldBudget {
        FieldBudget {
            left: it.heap().len().saturating_mul(FIELD_BUDGET_FACTOR),
            skipped: 0,
        }
    }

    /// True when a field of `len` bytes still fits; a refused field is counted.
    pub fn take(&mut self, len: usize) -> bool {
        if len > self.left {
            self.skipped += 1;
            return false;
        }
        self.left -= len;
        true
    }
}

/// Result of [`decode_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    Utf16(String),
    Utf8(String),
    Binary,
}

/// Find every item envelope in `data`.
///
/// At position `p`: `u32 size`, `05 00`, `u16 fixed_len`, `u32 size` again,
/// `u16 0`, `u16 type`; accepted when `16 <= fixed_len <= size` and the item
/// fits in `data`. After a match the scan resumes at `p + 16`, not after the
/// item, so items nested in another item's heap are found too.
///
/// Because items may overlap, a crafted value could make their total size
/// quadratic in the value length. An envelope is therefore accepted only
/// while the accepted items total at most [`ITEM_BUDGET_FACTOR`] times
/// `data.len()`; the rest are counted in [`Scan::skipped`].
pub fn scan_budgeted(data: &[u8]) -> Scan<'_> {
    let mut items = Vec::new();
    let mut skipped = 0usize;
    let mut left = data.len().saturating_mul(ITEM_BUDGET_FACTOR);
    let mut p = 0usize;
    while p + 16 <= data.len() {
        if let Some(item) = envelope_at(data, p) {
            if item.bytes.len() <= left {
                left -= item.bytes.len();
                items.push(item);
            } else {
                skipped += 1;
            }
            p += 16;
        } else {
            p += 1;
        }
    }
    Scan { items, skipped }
}

/// The items of [`scan_budgeted`], for callers that do not report the count
/// of skipped envelopes.
pub fn scan(data: &[u8]) -> Vec<Item<'_>> {
    scan_budgeted(data).items
}

fn envelope_at(data: &[u8], p: usize) -> Option<Item<'_>> {
    if data[p + 4] != 5 || data[p + 5] != 0 {
        return None;
    }
    let size = u32_at(data, p)?;
    if u32_at(data, p + 8)? != size || u16_at(data, p + 12)? != 0 {
        return None;
    }
    let fixed_len = u16_at(data, p + 6)? as usize;
    let size = size as usize;
    let start = p + 4;
    let end = start.checked_add(size)?;
    if fixed_len < 16 || fixed_len > size || end > data.len() {
        return None;
    }
    Some(Item {
        type_id: u16_at(data, p + 14)?,
        fixed_len,
        bytes: &data[start..end],
        offset: start,
    })
}

impl<'a> Item<'a> {
    /// The heap: everything after the fixed part.
    pub fn heap(&self) -> &'a [u8] {
        &self.bytes[self.fixed_len..]
    }

    /// Size of heap A: the `u32` at item offset 0x68, clamped to the heap
    /// length; 0 when the fixed part does not reach past 0x6c.
    pub fn heap_a_len(&self) -> usize {
        if self.fixed_len < HEAP_A_LEN_AT + 4 {
            return 0;
        }
        let n = u32_at(self.bytes, HEAP_A_LEN_AT).unwrap_or(0) as usize;
        n.min(self.heap().len())
    }

    /// Heap A: the first [`Item::heap_a_len`] heap bytes (unflagged fields).
    pub fn heap_a(&self) -> &'a [u8] {
        &self.heap()[..self.heap_a_len()]
    }

    /// Heap B: the heap after heap A (flagged fields).
    pub fn heap_b(&self) -> &'a [u8] {
        &self.heap()[self.heap_a_len()..]
    }

    /// The descriptor at item offset `o`: `(offset, len, flagged)`, when it
    /// lies inside the fixed part and its length is non-zero.
    fn descriptor(&self, o: usize) -> Option<(usize, usize, bool)> {
        if o.checked_add(8)? > self.fixed_len {
            return None;
        }
        let off = u32_at(self.bytes, o)? as usize;
        let v = u32_at(self.bytes, o + 4)?;
        let len = (v & !DESC_FLAG) as usize;
        (len != 0).then_some((off, len, v & DESC_FLAG != 0))
    }

    /// The bytes named by the descriptor at item offset `o`: from heap A when
    /// bit 31 of its length word is clear, from heap B when it is set. `None`
    /// when the descriptor is outside the fixed part, its length is zero, or
    /// the named range is outside its heap.
    pub fn var_at(&self, o: usize) -> Option<&'a [u8]> {
        let (off, len, flagged) = self.descriptor(o)?;
        let heap = if flagged {
            self.heap_b()
        } else {
            self.heap_a()
        };
        heap.get(off..off.checked_add(len)?)
    }

    /// Every flagged descriptor in the fixed part whose data fits heap B,
    /// found by a scan from offset 16, step 8 after a hit, else 1. Unflagged (heap A) descriptors are not detected: without
    /// a schema they cannot be told apart from plain data.
    pub fn descriptors(&self) -> Vec<VarField<'a>> {
        let mut out = Vec::new();
        let mut o = DESC_START;
        while o + 8 <= self.fixed_len {
            let data = match self.descriptor(o) {
                Some((_, _, true)) => self.var_at(o),
                _ => None,
            };
            if let Some(data) = data {
                out.push(VarField { at: o, data });
                o += 8;
            } else {
                o += 1;
            }
        }
        out
    }

    /// Unix seconds of the ticks at item offset `at` inside the fixed part;
    /// `None` when out of bounds or outside `window` (see [`tick_window`]).
    pub fn ticks_at(&self, at: usize, window: (u64, u64)) -> Option<i64> {
        if at.checked_add(8)? > self.fixed_len {
            return None;
        }
        let t = u64_at(self.bytes, at)?;
        (window.0..=window.1).contains(&t).then(|| ticks_to_unix(t))
    }
}

/// Unix seconds for .NET `ticks` (floor division, so pre-1970 is negative).
pub fn ticks_to_unix(ticks: u64) -> i64 {
    (ticks as i128 - UNIX_EPOCH_TICKS as i128).div_euclid(TICKS_PER_SECOND as i128) as i64
}

fn unix_to_ticks(secs: u64) -> u64 {
    UNIX_EPOCH_TICKS + secs * TICKS_PER_SECOND
}

/// Plausible tick range for a run at Unix time `now_secs`: 2000-01-01 up to
/// `now_secs` + 5 x 365 days.
pub fn tick_window_at(now_secs: u64) -> (u64, u64) {
    (
        unix_to_ticks(UNIX_2000),
        unix_to_ticks(now_secs.max(UNIX_2000) + AHEAD_SECONDS),
    )
}

/// [`tick_window_at`] the system clock. Call it once per run and pass the
/// result down, so every field is judged against the same ceiling.
pub fn tick_window() -> (u64, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(UNIX_2000);
    tick_window_at(now)
}

/// True when at least 90% of `total` are good.
fn mostly(good: usize, total: usize) -> bool {
    total > 0 && good * 10 >= total * 9
}

fn utf8_printable(c: char) -> bool {
    !c.is_control() || matches!(c, '\n' | '\r' | '\t')
}

fn utf16_good(c: char) -> bool {
    let cp = c as u32;
    let private_use = (0xE000..=0xF8FF).contains(&cp);
    let nonchar = (0xFDD0..=0xFDEF).contains(&cp) || cp & 0xFFFE == 0xFFFE;
    utf8_printable(c) && !private_use && !nonchar && c != '\u{FFFD}'
}

/// Decode a variable field as text.
///
/// 1. UTF-8: drop trailing `00` bytes; the rest must be non-empty, free of
///    `00`, valid UTF-8 and >= 90% printable. Tried first because even-length
///    ASCII/HTML is also valid UTF-16 and would decode as CJK garbage.
/// 2. UTF-16LE: the original input must have even length; drop trailing
///    `00 00` pairs; the rest must be non-empty, valid and >= 90% "good"
///    (not control except `\n \r \t`, private use U+E000..U+F8FF,
///    noncharacter or U+FFFD).
/// 3. Otherwise `Binary`.
///
/// Known limitation: a short UTF-16 string whose bytes are all printable
/// ASCII (e.g. U+4E4E = `4E 4E`) is read as UTF-8.
pub fn decode_text(b: &[u8]) -> Text {
    let trimmed = &b[..b.len() - b.iter().rev().take_while(|&&x| x == 0).count()];
    if !trimmed.is_empty() && !trimmed.contains(&0) {
        if let Ok(s) = std::str::from_utf8(trimmed) {
            let total = s.chars().count();
            if mostly(s.chars().filter(|&c| utf8_printable(c)).count(), total) {
                return Text::Utf8(s.to_owned());
            }
        }
    }

    if b.len().is_multiple_of(2) {
        let mut units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        while units.last() == Some(&0) {
            units.pop();
        }
        if let Ok(s) = String::from_utf16(&units) {
            let total = s.chars().count();
            if mostly(s.chars().filter(|&c| utf16_good(c)).count(), total) {
                return Text::Utf16(s);
            }
        }
    }
    Text::Binary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        make_item, make_item_heaps, make_node_pack, overlapping_envelopes, utf16, window, NOW,
        T2024,
    };

    #[test]
    fn item_round_trip() {
        let v = make_item(0xd2, 0x40, &[(0x10, b"name"), (0x18, b"x@y.z")], &[]);
        let items = scan(&v);
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.type_id, 0xd2);
        assert_eq!(it.fixed_len, 0x40);
        assert_eq!(it.offset, 4);
        assert_eq!(it.bytes, &v[4..]);
        assert_eq!(it.heap(), b"namex@y.z");
    }

    #[test]
    fn two_items_back_to_back() {
        let a = make_item(0x55, 0x20, &[], &[]);
        let b = make_item(0xc9, 0x30, &[(0x10, b"hello")], &[]);
        let v = [a.clone(), b.clone()].concat();
        let items = scan(&v);
        let got: Vec<(u16, usize)> = items.iter().map(|i| (i.type_id, i.offset)).collect();
        assert_eq!(got, [(0x55, 4), (0xc9, a.len() + 4)]);
        assert_eq!(items[1].bytes, &b[4..]);
    }

    #[test]
    fn item_inside_node_pack_section_a() {
        let it = make_item(0x4d, 0x28, &[(0x10, b"view")], &[]);
        let v = make_node_pack(&it, 1, &[]);
        let items = scan(&v);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].type_id, 0x4d);
        assert_eq!(items[0].offset, 8 + 4);
        assert_eq!(items[0].var_at(0x10), Some(&b"view"[..]));
    }

    #[test]
    fn item_nested_in_heap() {
        let inner = make_item(0xd2, 0x20, &[(0x10, b"in")], &[]);
        let outer = make_item(0xca, 0x40, &[(0x10, &inner)], &[]);
        let items = scan(&outer);
        let got: Vec<(u16, usize)> = items.iter().map(|i| (i.type_id, i.offset)).collect();
        assert_eq!(got, [(0xca, 4), (0xd2, 4 + 0x40 + 4)]);
        assert_eq!(items[1].bytes, &inner[4..]);
        assert_eq!(items[0].var_at(0x10), Some(&inner[..]));
    }

    #[test]
    fn descriptors_and_ticks() {
        let v = make_item(
            0xd2,
            0x60,
            &[(0x18, b"Display"), (0x28, b"a@b.c")],
            &[(0x40, T2024)],
        );
        let it = &scan(&v)[0];
        let d = it.descriptors();
        let got: Vec<(usize, &[u8])> = d.iter().map(|f| (f.at, f.data)).collect();
        assert_eq!(got, [(0x18, &b"Display"[..]), (0x28, &b"a@b.c"[..])]);
        assert_eq!(it.var_at(0x28), Some(&b"a@b.c"[..]));
        assert_eq!(it.var_at(0x20), None);
        assert_eq!(it.ticks_at(0x40, window()), Some(1_709_251_200));
        // zero bytes are not a tick inside the window
        assert_eq!(it.ticks_at(0x48, window()), None);
        // out of the fixed part
        assert_eq!(it.ticks_at(0x5c, window()), None);
        assert_eq!(it.var_at(0x5c), None);
        assert_eq!(it.var_at(usize::MAX), None);
    }

    #[test]
    fn zero_length_descriptor_is_none() {
        let mut v = make_item(0xd2, 0x30, &[(0x10, b"abc")], &[]);
        // length word at item offset 0x14 -> value offset 4 + 0x14
        v[0x18..0x1c].copy_from_slice(&DESC_FLAG.to_le_bytes());
        let it = &scan(&v)[0];
        assert_eq!(it.var_at(0x10), None);
        assert!(it.descriptors().is_empty());
    }

    #[test]
    fn descriptor_outside_heap_is_ignored() {
        let mut v = make_item(0xd2, 0x30, &[(0x10, b"abc")], &[]);
        // heap is 3 bytes; claim 4
        v[0x18..0x1c].copy_from_slice(&(DESC_FLAG | 4).to_le_bytes());
        let it = &scan(&v)[0];
        assert_eq!(it.var_at(0x10), None);
        assert!(it.descriptors().is_empty());
        // offset past the heap
        let mut v = make_item(0xd2, 0x30, &[(0x10, b"abc")], &[]);
        v[0x14..0x18].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(scan(&v)[0].descriptors().is_empty());
    }

    #[test]
    fn unflagged_field_in_heap_a_and_flagged_fields_in_heap_b() {
        let v = make_item_heaps(
            0xca,
            0x80,
            &[(0x70, b"AAAA")],
            &[(0x10, b"first"), (0x18, b"second")],
            &[],
        );
        let it = &scan(&v)[0];
        assert_eq!(it.heap_a_len(), 4);
        assert_eq!(it.heap_a(), b"AAAA");
        assert_eq!(it.heap_b(), b"firstsecond");
        assert_eq!(it.var_at(0x70), Some(&b"AAAA"[..]));
        assert_eq!(it.var_at(0x10), Some(&b"first"[..]));
        assert_eq!(it.var_at(0x18), Some(&b"second"[..]));
        // Generic discovery finds only the flagged ones, read from heap B.
        let got: Vec<(usize, &[u8])> = it.descriptors().iter().map(|f| (f.at, f.data)).collect();
        assert_eq!(got, [(0x10, &b"first"[..]), (0x18, &b"second"[..])]);
        // The field budget still counts the whole heap.
        let mut b = FieldBudget::new(it);
        assert!(b.take(15 * FIELD_BUDGET_FACTOR));
        assert!(!b.take(1));
    }

    #[test]
    fn flagged_field_is_read_from_heap_b_not_the_heap_start() {
        // Regression: the old rule read every field from the heap start, so
        // a flagged field at offset 0 returned heap A's bytes.
        let v = make_item_heaps(0xc9, 0x80, &[(0x70, b"xxxx")], &[(0x10, b"mail")], &[]);
        let it = &scan(&v)[0];
        assert_eq!(it.heap(), b"xxxxmail");
        assert_eq!(it.var_at(0x10), Some(&b"mail"[..]));
        assert_eq!(it.descriptors()[0].data, b"mail");
    }

    #[test]
    fn heap_bounds_are_checked() {
        // A_len larger than the heap: clamped, heap B is empty.
        let mut v = make_item_heaps(0xca, 0x80, &[(0x70, b"ab")], &[(0x10, b"cd")], &[]);
        v[4 + 0x68..4 + 0x6c].copy_from_slice(&100u32.to_le_bytes());
        let it = &scan(&v)[0];
        assert_eq!(it.heap_a_len(), 4);
        assert_eq!(it.heap_b(), b"");
        assert_eq!(it.var_at(0x10), None);
        assert!(it.descriptors().is_empty());
        assert_eq!(it.var_at(0x70), Some(&b"ab"[..]));

        // An unflagged descriptor past heap A (it would fit the whole heap).
        let mut v = make_item_heaps(0xca, 0x80, &[(0x70, b"ab")], &[(0x10, b"cd")], &[]);
        v[4 + 0x74..4 + 0x78].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(scan(&v)[0].var_at(0x70), None);
        v[4 + 0x70..4 + 0x78].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0]);
        assert_eq!(scan(&v)[0].var_at(0x70), None);

        // A flagged descriptor past heap B.
        let mut v = make_item_heaps(0xca, 0x80, &[(0x70, b"ab")], &[(0x10, b"cd")], &[]);
        v[4 + 0x10..4 + 0x14].copy_from_slice(&1u32.to_le_bytes());
        let it = &scan(&v)[0];
        assert_eq!(it.var_at(0x10), None);
        assert!(it.descriptors().is_empty());

        // fixed_len < 0x6c: no A_len, heap A is empty, an unflagged
        // descriptor names nothing.
        let mut v = make_item(0xd2, 0x68, &[(0x10, b"abc")], &[]);
        v[4 + 0x60..4 + 0x68].copy_from_slice(&[0, 0, 0, 0, 3, 0, 0, 0]);
        let it = &scan(&v)[0];
        assert_eq!(it.heap_a_len(), 0);
        assert_eq!(it.heap_b(), b"abc");
        assert_eq!(it.var_at(0x60), None);
        assert_eq!(it.var_at(0x10), Some(&b"abc"[..]));
    }

    #[test]
    fn size_mismatch_is_not_an_item() {
        let mut v = make_item(0xd2, 0x20, &[(0x10, b"abc")], &[]);
        let second = u32::from_le_bytes(v[8..12].try_into().unwrap()) - 1;
        v[8..12].copy_from_slice(&second.to_le_bytes());
        assert!(scan(&v).is_empty());
        // prefix claims more than the buffer holds
        let mut v = make_item(0xd2, 0x20, &[], &[]);
        v[0..4].copy_from_slice(&0x21u32.to_le_bytes());
        v[8..12].copy_from_slice(&0x21u32.to_le_bytes());
        assert!(scan(&v).is_empty());
        // fixed_len below 16 or above size
        let mut v = make_item(0xd2, 0x20, &[], &[]);
        v[6..8].copy_from_slice(&15u16.to_le_bytes());
        assert!(scan(&v).is_empty());
        v[6..8].copy_from_slice(&0x21u16.to_le_bytes());
        assert!(scan(&v).is_empty());
        assert!(scan(&[]).is_empty());
        assert!(scan(&[5, 0, 0]).is_empty());
    }

    #[test]
    fn utf16_scripts() {
        for s in ["שלום עולם", "Привет мир", "日本語の文字", "hi 😀 there"]
        {
            assert_eq!(decode_text(&utf16(s)), Text::Utf16(s.into()), "{s}");
        }
    }

    #[test]
    fn utf16_trailing_nul() {
        assert_eq!(
            decode_text(&[0x48, 0x00, 0x69, 0x00, 0x00, 0x00]),
            Text::Utf16("Hi".into())
        );
    }

    #[test]
    fn utf16_multiline_is_text() {
        // Half of the chars are '\n': only passes if newlines count as good.
        let s = "a\nb\nc\nd\n";
        assert_eq!(decode_text(&utf16(s)), Text::Utf16(s.into()));
        let s = "x\r\n\ty\r\n";
        assert_eq!(decode_text(&utf16(s)), Text::Utf16(s.into()));
    }

    #[test]
    fn html_is_utf8() {
        let even = b"<b>hello</b>";
        assert_eq!(even.len() % 2, 0);
        assert_eq!(decode_text(even), Text::Utf8("<b>hello</b>".into()));
        let odd = b"<p>odd!</p>";
        assert_eq!(odd.len() % 2, 1);
        assert_eq!(decode_text(odd), Text::Utf8("<p>odd!</p>".into()));
        assert_eq!(
            decode_text(b"<p>x</p>\n\0\0"),
            Text::Utf8("<p>x</p>\n".into())
        );
        assert_eq!(
            decode_text("קפה ☕".as_bytes()),
            Text::Utf8("קפה ☕".into())
        );
    }

    #[test]
    fn private_use_and_control_is_binary() {
        let b = [0x01, 0xe0, 0x02, 0xe0, 0x03, 0xe0, 0x1f, 0x00];
        assert_eq!(decode_text(&b), Text::Binary);
        assert_eq!(decode_text(&[]), Text::Binary);
        assert_eq!(decode_text(&[0, 0, 0, 0]), Text::Binary);
        // lone surrogate
        assert_eq!(decode_text(&[0x00, 0xd8, 0x41, 0x00]), Text::Binary);
    }

    #[test]
    fn ticks_conversion_and_window() {
        assert_eq!(ticks_to_unix(UNIX_EPOCH_TICKS), 0);
        assert_eq!(ticks_to_unix(T2024), 1_709_251_200);
        assert_eq!(ticks_to_unix(UNIX_EPOCH_TICKS - 1), -1);
        let (lo, hi) = tick_window_at(NOW);
        assert_eq!(ticks_to_unix(lo), 946_684_800);
        assert!((lo..=hi).contains(&T2024));
        assert_eq!(ticks_to_unix(hi), (NOW + 5 * 365 * 86_400) as i64);
        // A clock before 2000 is clamped to the lower bound.
        assert_eq!(
            ticks_to_unix(tick_window_at(0).1),
            946_684_800 + 5 * 365 * 86_400
        );
        // The system-clock window has the same floor and a later ceiling.
        let (sys_lo, sys_hi) = tick_window();
        assert_eq!(sys_lo, lo);
        assert!(sys_hi > sys_lo);
    }

    #[test]
    fn overlapping_envelopes_stop_at_the_budget() {
        let len = 1 << 20;
        let v = overlapping_envelopes(len);
        let s = scan_budgeted(&v);
        let total: usize = s.items.iter().map(|i| i.bytes.len()).sum();
        assert!(total <= len * ITEM_BUDGET_FACTOR, "{total}");
        // Without the budget every envelope is accepted: len / 16 items
        // totalling about len^2 / 32 bytes.
        assert_eq!(s.items.len() + s.skipped, len / 16 - 1);
        assert!(s.skipped > len / 16 - 64, "{}", s.skipped);
        assert_eq!(s.items[0].bytes.len(), len - 4);
    }

    #[test]
    fn nested_and_adjacent_items_fit_the_budget() {
        let inner = make_item(0xd2, 0x20, &[(0x10, b"in")], &[]);
        let mid = make_item(0xca, 0x40, &[(0x10, &inner)], &[]);
        let outer = make_item(0xc9, 0x40, &[(0x10, &mid)], &[]);
        let v = [outer.clone(), outer].concat();
        let s = scan_budgeted(&v);
        assert_eq!((s.items.len(), s.skipped), (6, 0));
    }

    #[test]
    fn field_budget_refuses_past_four_heaps() {
        let v = make_item(0x56, 0x40, &[(0x10, b"0123456789")], &[]);
        let it = &scan(&v)[0];
        let mut b = FieldBudget::new(it);
        assert!((0..FIELD_BUDGET_FACTOR).all(|_| b.take(10)));
        assert!(!b.take(1));
        assert!(b.take(0));
        assert_eq!(b.skipped, 1);
    }
}
