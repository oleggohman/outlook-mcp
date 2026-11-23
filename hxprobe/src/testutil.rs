//! Synthetic fixtures for unit tests: valid blocks and stores built in code,
//! so no real mail ever enters the repository.

use crate::block::{crc32, HEADER, MAGIC};
use crate::item::{self, TICKS_PER_SECOND, UNIX_EPOCH_TICKS};
use crate::props::PropValue;
use crate::props_gen::TYPES;
use crate::value::{ObjRef, COLLECTION_REF_TYPE, ENTRY_VERSION, MIN_ENTRY_LEN};

/// Encode `data` as a single literal-only LZ4 sequence.
///
/// Token `min(len, 15) << 4`; when `len >= 15` the 255-continuation for
/// `len - 15` always follows, ending in a byte below `0xFF` (`0x00` when
/// nothing remains, so 15 encodes as `F0 00`). Then the raw bytes.
pub fn lz4_literals(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut out = vec![(len.min(15) as u8) << 4];
    if len >= 15 {
        let mut rest = len - 15;
        while rest >= 255 {
            out.push(0xFF);
            rest -= 255;
        }
        out.push(rest as u8);
    }
    out.extend_from_slice(data);
    out
}

/// Assemble a block from explicit header values.
///
/// Both CRCs are computed over whatever the header says, so a test can build a
/// block that is checksum-valid but otherwise wrong (bad key length, codec or
/// inflated length). The body CRC range is clamped to the bytes written.
pub fn make_block_raw(
    key: &[u8],
    value: &[u8],
    key_len: u32,
    inflated_len: u32,
    codec: u32,
) -> Vec<u8> {
    let payload = lz4_literals(value);
    let mut b = vec![0u8; HEADER];
    b[0x08..0x10].copy_from_slice(&MAGIC.to_le_bytes());
    b[0x10..0x14].copy_from_slice(&key_len.to_le_bytes());
    b[0x14..0x18].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    b[0x18..0x1c].copy_from_slice(&inflated_len.to_le_bytes());
    b[0x1c..0x20].copy_from_slice(&codec.to_le_bytes());
    b.extend_from_slice(key);
    b.extend_from_slice(&payload);

    let end = (HEADER + key_len as usize + payload.len()).min(b.len());
    let body = crc32(&b[0x08..end]);
    b[0x04..0x08].copy_from_slice(&body.to_le_bytes());
    let head = crc32(&b[0x04..HEADER]);
    b[0x00..0x04].copy_from_slice(&head.to_le_bytes());
    b
}

/// A valid block for `key` (8 or 16 bytes) holding `value`.
///
/// `value` must be non-empty: the parser rejects `inflated_len == 0`.
pub fn make_block(key: &[u8], value: &[u8]) -> Vec<u8> {
    assert!(!value.is_empty(), "block values must be non-empty");
    make_block_raw(key, value, key.len() as u32, value.len() as u32, 4)
}

/// A store file: a `Nostromo` header (version `i`, page size 4096) followed
/// by `blocks`, each starting on a 512-byte boundary.
pub fn make_store(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[..8].copy_from_slice(crate::header::MAGIC);
    s[8] = b'i';
    s[0x38..0x40].copy_from_slice(&4096u64.to_le_bytes());
    for b in blocks {
        s.extend_from_slice(b);
        s.resize(s.len().next_multiple_of(512), 0);
    }
    s
}

fn obj_ref_bytes(r: ObjRef) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.extend_from_slice(&r.prefix.to_le_bytes());
    b.extend_from_slice(&r.page.to_le_bytes());
    b.extend_from_slice(&r.type_id.to_le_bytes());
    b
}

/// A node pack value: section A with `count_a`, then section B holding
/// `entries` back to back (built with [`make_collection_entry`]).
pub fn make_node_pack(section_a: &[u8], count_a: u32, entries: &[Vec<u8>]) -> Vec<u8> {
    let section_b = entries.concat();
    let mut d = Vec::new();
    d.extend_from_slice(&count_a.to_le_bytes());
    d.extend_from_slice(&(section_a.len() as u32).to_le_bytes());
    d.extend_from_slice(section_a);
    d.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    d.extend_from_slice(&(section_b.len() as u32).to_le_bytes());
    d.extend_from_slice(&section_b);
    d
}

/// A section B entry: version 6, `0x570`, `page`, `type_id`, entry size,
/// owner ref, then `body`.
pub fn make_collection_entry(type_id: u16, page: u64, owner: ObjRef, body: &[u8]) -> Vec<u8> {
    let size = (MIN_ENTRY_LEN + body.len()) as u32;
    let mut e = Vec::new();
    e.extend_from_slice(&ENTRY_VERSION.to_le_bytes());
    e.extend_from_slice(&0x570u16.to_le_bytes());
    e.extend_from_slice(&page.to_le_bytes());
    e.extend_from_slice(&type_id.to_le_bytes());
    e.extend_from_slice(&size.to_le_bytes());
    e.extend_from_slice(&obj_ref_bytes(owner));
    e.extend_from_slice(body);
    e
}

/// A collection page value stored under `key`: `u32 1`, the self ref
/// `{prefix 0, key, 0x37}`, `member_count`, then `body`.
pub fn make_collection_page(key: u64, member_count: u32, body: &[u8]) -> Vec<u8> {
    let mut d = 1u32.to_le_bytes().to_vec();
    d.extend_from_slice(&obj_ref_bytes(ObjRef {
        prefix: 0,
        page: key,
        type_id: COLLECTION_REF_TYPE,
    }));
    d.extend_from_slice(&member_count.to_le_bytes());
    d.extend_from_slice(body);
    d
}

/// A `u32 size` prefix followed by an object item of `type_id`.
///
/// The fixed part is `fixed_len` bytes (header included, zero filled). Each
/// `(at, data)` in `vars` writes a flagged descriptor at item offset `at` and
/// appends `data` to heap B, in the order given; heap A stays empty. Each
/// `(at, ticks)` writes a `u64` at item offset `at`.
pub fn make_item(
    type_id: u16,
    fixed_len: usize,
    vars: &[(usize, &[u8])],
    ticks: &[(usize, u64)],
) -> Vec<u8> {
    make_item_heaps(type_id, fixed_len, &[], vars, ticks)
}

/// [`make_item`] with both heaps: each `(at, data)` in `vars_a` writes an
/// unflagged descriptor and appends `data` to heap A, each one in `vars_b` a
/// flagged descriptor and appends `data` to heap B. When heap A is not empty
/// its length is written to `A_len` at item offset 0x68.
pub fn make_item_heaps(
    type_id: u16,
    fixed_len: usize,
    vars_a: &[(usize, &[u8])],
    vars_b: &[(usize, &[u8])],
    ticks: &[(usize, u64)],
) -> Vec<u8> {
    assert!(fixed_len >= 16, "fixed_len must cover the item header");
    let mut item = vec![0u8; fixed_len];
    let mut heaps = [Vec::new(), Vec::new()];
    for (heap, vars, flag) in [(0, vars_a, 0), (1, vars_b, 0x8000_0000u32)] {
        let heap = &mut heaps[heap];
        for &(at, data) in vars {
            assert!(at + 8 <= fixed_len, "descriptor outside the fixed part");
            item[at..at + 4].copy_from_slice(&(heap.len() as u32).to_le_bytes());
            item[at + 4..at + 8].copy_from_slice(&(data.len() as u32 | flag).to_le_bytes());
            heap.extend_from_slice(data);
        }
    }
    for &(at, t) in ticks {
        assert!(at + 8 <= fixed_len, "ticks outside the fixed part");
        item[at..at + 8].copy_from_slice(&t.to_le_bytes());
    }
    let [heap_a, heap_b] = heaps;
    if !heap_a.is_empty() {
        assert!(fixed_len >= 0x6c, "A_len outside the fixed part");
        item[0x68..0x6c].copy_from_slice(&(heap_a.len() as u32).to_le_bytes());
    }
    item.extend_from_slice(&heap_a);
    item.extend_from_slice(&heap_b);
    let size = item.len() as u32;
    item[0..2].copy_from_slice(&5u16.to_le_bytes());
    item[2..4].copy_from_slice(&(fixed_len as u16).to_le_bytes());
    item[4..8].copy_from_slice(&size.to_le_bytes());
    item[8..10].copy_from_slice(&0u16.to_le_bytes());
    item[10..12].copy_from_slice(&type_id.to_le_bytes());
    let mut out = size.to_le_bytes().to_vec();
    out.extend_from_slice(&item);
    out
}

/// A `u32 size` prefix followed by an item of the schema type `type_name`
/// with the schema's `fixed_len` and `guid` at item offset 12.
///
/// Each `(name, value)` is written at the property's schema position: `Bool`
/// as bit `ci` of bitmap A; `Text` as a descriptor plus heap bytes (UTF-16LE
/// in heap B for dt 7, UTF-8 in heap A otherwise); `Bytes` as a descriptor
/// plus heap A bytes, or
/// raw bytes for a fixed-size property; `Int` and `Real` little-endian in the
/// property's size; `Time` (unix seconds) as .NET ticks; `Guid` as 16 bytes;
/// `Ref` as `u16 version 1, u16 type_id, guid`, zero padded to 48 bytes.
/// `Missing` and `Skipped` write nothing. Everything else stays zero.
pub fn make_typed_item(type_name: &str, guid: [u8; 16], values: &[(&str, PropValue)]) -> Vec<u8> {
    let t = TYPES
        .iter()
        .find(|t| t.name == type_name)
        .unwrap_or_else(|| panic!("no schema type {type_name}"));
    let prop = |name: &str| {
        *t.props
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{type_name} has no property {name}"))
    };

    // Variable fields first: make_item_heaps lays out descriptors and heaps.
    // As in the store, dt 7 strings go to heap B (flagged), other variable
    // fields to heap A (unflagged).
    let mut heap_values: Vec<(bool, usize, Vec<u8>)> = Vec::new();
    for (name, v) in values {
        let p = prop(name);
        if p.size != 0 {
            continue;
        }
        let data = match v {
            PropValue::Text(s) if p.dt == 7 => utf16(s),
            PropValue::Text(s) => s.as_bytes().to_vec(),
            PropValue::Bytes(b) => b.clone(),
            PropValue::Missing | PropValue::Skipped => continue,
            other => panic!("{name}: {other:?} is not a variable field value"),
        };
        heap_values.push((p.dt == 7, p.offset as usize, data));
    }
    let vars = |in_b: bool| -> Vec<(usize, &[u8])> {
        heap_values
            .iter()
            .filter(|(b, ..)| *b == in_b)
            .map(|(_, at, d)| (*at, &d[..]))
            .collect()
    };
    let mut out = make_item_heaps(
        t.type_id,
        t.fixed_len as usize,
        &vars(false),
        &vars(true),
        &[],
    );

    // Item offset `o` is at `out[4 + o]`, after the size prefix.
    let item = &mut out[4..];
    item[12..28].copy_from_slice(&guid);
    for (name, v) in values {
        let p = prop(name);
        if p.size == 0 {
            continue;
        }
        if p.offset < 0 {
            if let PropValue::Bool(true) = v {
                let at = t.fields_end as usize + usize::from(p.ci / 8);
                item[at] |= 1 << (p.ci % 8);
            }
            continue;
        }
        let at = p.offset as usize;
        let size = usize::from(p.size);
        let bytes: Vec<u8> = match v {
            PropValue::Int(i) => i.to_le_bytes()[..size.min(8)].to_vec(),
            PropValue::Real(f) => f.to_le_bytes().to_vec(),
            PropValue::Time(s) => (UNIX_EPOCH_TICKS as i64 + s * TICKS_PER_SECOND as i64)
                .to_le_bytes()
                .to_vec(),
            PropValue::Guid(g) => g.to_vec(),
            PropValue::Ref { type_id, guid } => {
                let mut r = vec![0u8; 48];
                r[0..2].copy_from_slice(&1u16.to_le_bytes());
                r[2..4].copy_from_slice(&type_id.to_le_bytes());
                r[4..20].copy_from_slice(guid);
                r
            }
            PropValue::Bytes(b) => b.clone(),
            PropValue::Missing | PropValue::Skipped | PropValue::Bool(false) => continue,
            other => panic!("{name}: {other:?} does not fit size {size}"),
        };
        assert!(bytes.len() <= size, "{name}: value wider than the property");
        item[at..at + bytes.len()].copy_from_slice(&bytes);
    }
    out
}

/// Overwrite the envelope `fixed_len` of a prefixed item (as built by
/// [`make_item`] or [`make_typed_item`]); the bytes stay where they are.
pub fn set_fixed_len(v: &mut [u8], fixed_len: u16) {
    v[6..8].copy_from_slice(&fixed_len.to_le_bytes());
}

/// Copy the 8-byte descriptor at item offset `from` of a prefixed item to
/// item offset `to`, so both name the same heap bytes.
pub fn copy_descriptor(v: &mut [u8], from: usize, to: usize) {
    let d: [u8; 8] = v[4 + from..4 + from + 8].try_into().unwrap();
    v[4 + to..4 + to + 8].copy_from_slice(&d);
}

/// 2024-03-01 00:00:00 UTC in .NET ticks.
pub const T2024: u64 = UNIX_EPOCH_TICKS + 1_709_251_200 * TICKS_PER_SECOND;

/// The fixed "current time" of the tests, 2026-01-01 00:00:00 UTC, so no
/// test depends on the wall clock.
pub const NOW: u64 = 1_767_225_600;

/// The tick window at [`NOW`].
pub fn window() -> (u64, u64) {
    item::tick_window_at(NOW)
}

/// `s` as UTF-16LE bytes.
pub fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// The 8-byte block key of page `k`.
pub fn page(k: u64) -> [u8; 8] {
    k.to_le_bytes()
}

/// A Recipient item with the schema's fixed_len (0x15a): name, email and a 2024 tick.
pub fn recipient(name: &str, email: &str) -> Vec<u8> {
    make_item(
        0xd2,
        0x15a,
        &[(0x100, &utf16(name)), (0x108, &utf16(email))],
        &[(0xf8, T2024)],
    )
}

/// A value of `len` bytes (a multiple of 16) with an item envelope of type
/// 0x56 every 16 bytes, each claiming everything up to the end of the value:
/// `len / 16 - 1` valid, overlapping items of about `len^2 / 32` bytes in all.
pub fn overlapping_envelopes(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for p in (0..len - 16).step_by(16) {
        let size = (len - p - 4) as u32;
        v[p..p + 4].copy_from_slice(&size.to_le_bytes());
        v[p + 4..p + 6].copy_from_slice(&[5, 0]);
        v[p + 6..p + 8].copy_from_slice(&16u16.to_le_bytes());
        v[p + 8..p + 12].copy_from_slice(&size.to_le_bytes());
        v[p + 14..p + 16].copy_from_slice(&0x56u16.to_le_bytes());
    }
    v
}

/// A store of `n` 512-byte slots, each opening with a block header whose
/// header checksum is valid and whose value is claimed to run to the end of
/// the file. No body checksum agrees, so no block verifies, but checking them
/// all would checksum about `n / 2` times the file length.
pub fn overlapping_blocks(n: usize) -> Vec<u8> {
    let mut s = make_store(&[]);
    s.resize(512 + n * 512, 0);
    let len = s.len();
    for off in (512..len).step_by(512) {
        let val_len = (len - off - HEADER - 8) as u32;
        s[off + 0x08..off + 0x10].copy_from_slice(&MAGIC.to_le_bytes());
        s[off + 0x10..off + 0x14].copy_from_slice(&8u32.to_le_bytes());
        s[off + 0x14..off + 0x18].copy_from_slice(&val_len.to_le_bytes());
        s[off + 0x18..off + 0x1c].copy_from_slice(&1u32.to_le_bytes());
        s[off + 0x1c..off + 0x20].copy_from_slice(&4u32.to_le_bytes());
        let head = crc32(&s[off + 0x04..off + HEADER]);
        s[off..off + 4].copy_from_slice(&head.to_le_bytes());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_encoder_length_continuations() {
        assert_eq!(lz4_literals(&[1; 14])[0], 0xe0);
        assert_eq!(&lz4_literals(&[1; 15])[..2], &[0xf0, 0x00]);
        assert_eq!(&lz4_literals(&[1; 270])[..3], &[0xf0, 0xff, 0x00]);
        for len in [1, 14, 15, 16, 269, 270, 271, 600] {
            let v: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(crate::lz::decode_exact(&lz4_literals(&v), len).unwrap(), v);
        }
    }

    #[test]
    fn store_has_a_valid_header() {
        let s = make_store(&[]);
        let h = crate::header::check(&s).unwrap();
        assert!(h.known);
        assert_eq!(h.page_size, 4096);
    }
}
