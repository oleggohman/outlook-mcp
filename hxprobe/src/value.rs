//! Classification of live values stored under 8-byte (page-group) keys.
//!
//! Two kinds are known (see SPEC.md, "Values under 8-byte keys"):
//!
//! ```text
//! collection page:
//!   u32 1 | ObjRef { u16 prefix, u64 key, u16 0x37 } | u32 member_count | index entries...
//!
//! node pack:
//!   u32 count_a | u32 size_a | section A (size_a bytes)
//!   u32 count_b | u32 size_b | section B (size_b bytes)       8+size_a+8+size_b == len
//!
//! section B entry (count_b of them, walked by entry_size):
//!   +0x00 u16 version (6) | +0x02 u16 (0x570, not checked) | +0x04 u64 page
//!   +0x0c u16 collection_type | +0x0e u32 entry_size | +0x12 ObjRef owner | ...
//! ```
//!
//! Anything else is [`Value::Other`]. Classification never panics on
//! arbitrary input.

use crate::le::{u16_at, u32_at, u64_at};

/// Size of a serialized [`ObjRef`].
pub const OBJ_REF_LEN: usize = 12;
/// Object type of the ref that opens a collection page.
pub const COLLECTION_REF_TYPE: u16 = 0x37;
/// Required version of a section B entry.
pub const ENTRY_VERSION: u16 = 6;
/// Smallest section B entry: 18-byte entry header plus the owner ref.
pub const MIN_ENTRY_LEN: usize = 0x12 + OBJ_REF_LEN;

/// Reference to an object: `u16 prefix, u64 page, u16 type_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjRef {
    pub prefix: u16,
    pub page: u64,
    pub type_id: u16,
}

impl ObjRef {
    /// Decode the first 12 bytes of `b`; `None` when `b` is shorter.
    pub fn parse(b: &[u8]) -> Option<ObjRef> {
        if b.len() < OBJ_REF_LEN {
            return None;
        }
        Some(ObjRef {
            prefix: u16_at(b, 0)?,
            page: u64_at(b, 2)?,
            type_id: u16_at(b, 10)?,
        })
    }
}

/// One section B entry of a node pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionEntry<'a> {
    /// The entry's collection type.
    pub type_id: u16,
    /// Object that owns the collection.
    pub owner: ObjRef,
    /// The whole entry, header included (`entry_size` bytes).
    pub bytes: &'a [u8],
}

/// A classified page value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    NodePack {
        section_a: &'a [u8],
        count_a: u32,
        collections: Vec<CollectionEntry<'a>>,
    },
    CollectionPage {
        id: ObjRef,
        member_count: u32,
    },
    Other,
}

/// Classify the value `data` stored under page key `key`.
///
/// Collection page is tested first, then node pack, else `Other`.
pub fn classify(key: u64, data: &[u8]) -> Value<'_> {
    if let Some(v) = collection_page(key, data) {
        return v;
    }
    node_pack(data).unwrap_or(Value::Other)
}

fn collection_page(key: u64, data: &[u8]) -> Option<Value<'_>> {
    if u32_at(data, 0)? != 1 {
        return None;
    }
    let id = ObjRef::parse(data.get(4..)?)?;
    if id.page != key || id.type_id != COLLECTION_REF_TYPE {
        return None;
    }
    let member_count = u32_at(data, 4 + OBJ_REF_LEN)?;
    Some(Value::CollectionPage { id, member_count })
}

fn node_pack(data: &[u8]) -> Option<Value<'_>> {
    let count_a = u32_at(data, 0)?;
    let size_a = u32_at(data, 4)? as usize;
    let b_hdr = 8usize.checked_add(size_a)?;
    let count_b = u32_at(data, b_hdr)?;
    let size_b = u32_at(data, b_hdr + 4)? as usize;
    let b_start = b_hdr + 8;
    if b_start.checked_add(size_b)? != data.len() {
        return None;
    }
    let section_a = &data[8..b_hdr];
    let collections = walk_section_b(&data[b_start..], count_b)?;
    Some(Value::NodePack {
        section_a,
        count_a,
        collections,
    })
}

/// Walk exactly `count` entries; `None` unless every entry is well formed and
/// the walk ends exactly at the end of `sec` (no partial results).
fn walk_section_b(sec: &[u8], count: u32) -> Option<Vec<CollectionEntry<'_>>> {
    // Each entry takes at least MIN_ENTRY_LEN bytes, so a larger count cannot
    // fit; checking first also bounds the allocation below.
    if (count as usize) > sec.len() / MIN_ENTRY_LEN {
        return None;
    }
    let mut out = Vec::with_capacity(count as usize);
    let mut o = 0usize;
    for _ in 0..count {
        if u16_at(sec, o)? != ENTRY_VERSION {
            return None;
        }
        let type_id = u16_at(sec, o + 0x0c)?;
        let size = u32_at(sec, o + 0x0e)? as usize;
        if size < MIN_ENTRY_LEN {
            return None;
        }
        let end = o.checked_add(size)?;
        let bytes = sec.get(o..end)?;
        let owner = ObjRef::parse(&bytes[0x12..])?;
        out.push(CollectionEntry {
            type_id,
            owner,
            bytes,
        });
        o = end;
    }
    (o == sec.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{make_collection_entry, make_collection_page, make_node_pack};

    const OWNER: ObjRef = ObjRef {
        prefix: 0x0102,
        page: 0x1122_3344_5566_7788,
        type_id: 0x00c9,
    };

    fn entry(type_id: u16, body: &[u8]) -> Vec<u8> {
        make_collection_entry(type_id, 0xabcd, OWNER, body)
    }

    #[test]
    fn obj_ref_parses_twelve_bytes() {
        let b = [
            0x02, 0x01, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xc9, 0x00,
        ];
        assert_eq!(ObjRef::parse(&b), Some(OWNER));
        assert_eq!(ObjRef::parse(&b[..11]), None);
    }

    #[test]
    fn node_pack_without_collections() {
        let d = make_node_pack(b"section a bytes", 2, &[]);
        match classify(5, &d) {
            Value::NodePack {
                section_a,
                count_a,
                collections,
            } => {
                assert_eq!(section_a, b"section a bytes");
                assert_eq!(count_a, 2);
                assert!(collections.is_empty());
            }
            v => panic!("expected node pack, got {v:?}"),
        }
    }

    #[test]
    fn node_pack_with_one_collection_decodes_owner() {
        let e = entry(0x0d2, b"members");
        let d = make_node_pack(b"A", 1, std::slice::from_ref(&e));
        let Value::NodePack { collections, .. } = classify(5, &d) else {
            panic!("expected node pack");
        };
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].type_id, 0x0d2);
        assert_eq!(collections[0].owner, OWNER);
        assert_eq!(collections[0].bytes, &e[..]);
    }

    #[test]
    fn node_pack_with_three_collections() {
        let es = [entry(1, b""), entry(2, b"xy"), entry(3, &[0u8; 40])];
        let d = make_node_pack(&[], 0, &es);
        let Value::NodePack { collections, .. } = classify(5, &d) else {
            panic!("expected node pack");
        };
        let types: Vec<u16> = collections.iter().map(|c| c.type_id).collect();
        assert_eq!(types, [1, 2, 3]);
        for (c, e) in collections.iter().zip(&es) {
            assert_eq!(c.bytes, &e[..]);
            assert_eq!(c.owner, OWNER);
        }
    }

    #[test]
    fn collection_page_is_recognised() {
        let d = make_collection_page(0x77, 3, b"index entries");
        match classify(0x77, &d) {
            Value::CollectionPage { id, member_count } => {
                assert_eq!(id.page, 0x77);
                assert_eq!(id.type_id, COLLECTION_REF_TYPE);
                assert_eq!(member_count, 3);
            }
            v => panic!("expected collection page, got {v:?}"),
        }
        // The ref must name the key the value is stored under.
        assert!(!matches!(classify(0x78, &d), Value::CollectionPage { .. }));
    }

    #[test]
    fn size_mismatch_is_other() {
        let mut d = make_node_pack(b"A", 1, &[entry(1, b"")]);
        d.push(0);
        assert_eq!(classify(5, &d), Value::Other);
        d.truncate(d.len() - 2);
        assert_eq!(classify(5, &d), Value::Other);
    }

    #[test]
    fn overrunning_entry_is_other() {
        let mut e = entry(1, b"body");
        // entry_size one past the section end
        let size = (e.len() + 1) as u32;
        e[0x0e..0x12].copy_from_slice(&size.to_le_bytes());
        assert_eq!(classify(5, &make_node_pack(b"A", 1, &[e])), Value::Other);
    }

    #[test]
    fn too_small_entry_size_is_other() {
        let mut e = entry(1, b"");
        e[0x0e..0x12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(classify(5, &make_node_pack(b"A", 1, &[e])), Value::Other);
    }

    #[test]
    fn entry_version_five_is_other() {
        let mut e = entry(1, b"body");
        e[0..2].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(classify(5, &make_node_pack(b"A", 1, &[e])), Value::Other);
    }

    #[test]
    fn trailing_bytes_after_last_entry_is_other() {
        let mut d = make_node_pack(b"A", 1, &[entry(1, b"body")]);
        // Grow section B by 4 junk bytes and keep the outer sizes consistent,
        // so only the walk can notice.
        d.extend_from_slice(&[0xee; 4]);
        let b_size_at = 8 + 1 + 4;
        let size_b = u32::from_le_bytes(d[b_size_at..b_size_at + 4].try_into().unwrap()) + 4;
        d[b_size_at..b_size_at + 4].copy_from_slice(&size_b.to_le_bytes());
        assert_eq!(classify(5, &d), Value::Other);
    }

    #[test]
    fn wrong_entry_count_is_other() {
        let mut d = make_node_pack(b"A", 1, &[entry(1, b"")]);
        let count_b_at = 8 + 1;
        d[count_b_at..count_b_at + 4].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(classify(5, &d), Value::Other);
        d[count_b_at..count_b_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(classify(5, &d), Value::Other);
    }

    #[test]
    fn short_buffers_are_other() {
        for len in 0..16 {
            assert_eq!(
                classify(0, &vec![0u8; len][..len]),
                Value::Other,
                "len {len}"
            );
        }
        // A collection page cut before member_count.
        let d = make_collection_page(9, 1, b"");
        assert_eq!(classify(9, &d[..d.len() - 1]), Value::Other);
    }

    #[test]
    fn fixed_junk_is_other() {
        let junk: Vec<u8> = (0u32..512)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for cut in [1, 7, 8, 15, 16, 31, 100, 511, 512] {
            assert_eq!(classify(0x1234, &junk[..cut]), Value::Other, "cut {cut}");
        }
        let ff = [0xffu8; 64];
        assert_eq!(classify(u64::MAX, &ff), Value::Other);
    }
}
