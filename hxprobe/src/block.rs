//! The HxStore block container.
//!
//!   * `+0x04` holds `crc32(block+8, len-8)` and `+0x00` holds
//!     `crc32(block+4, 0x1c)`;
//!   * `[block+0x14]` is the value length, and `+0x04` is checked before the
//!     payload is touched.
//!
//! Every block is a key-value pair. The field at `+0x10` is the key length,
//! not a record type: the key sits between the 32-byte header and the value.
//! With this layout both checksums verify on 77,454 / 77,454 blocks of a live
//! store. That is what makes this a parser rather than a heuristic: a wrong
//! offset fails loudly instead of yielding plausible garbage.
//!
//! ```text
//!   +0x00  u32  crc32(block[0x04..0x20])
//!   +0x04  u32  crc32(block[0x08 .. 0x20 + key_len + val_len])
//!   +0x08  u64  magic 0x5d0245643b706a05
//!   +0x10  u32  key_len         8 (page-group id) or 16 (GUID-named record)
//!   +0x14  u32  val_len         compressed bytes, starting after the key
//!   +0x18  u32  inflated_len    exact size of the decompressed value
//!   +0x1c  u32  codec           4 = LZ4
//!   +0x20  ...  key (key_len bytes), then the LZ4-compressed value
//! ```
//!
//! Blocks start on 512-byte boundaries, and [`find_all`] ignores a signature
//! anywhere else.

use crate::lz;

/// Little-endian u64 that opens every block, at offset `+0x08`.
pub const MAGIC: u64 = 0x5d02_4564_3b70_6a05;

/// Size of the fixed block header; the key follows it.
pub const HEADER: usize = 0x20;

/// The only codec seen in a store: LZ4 block format.
pub const CODEC_LZ4: u32 = 4;

/// Every block starts on a multiple of this (77,454 of 77,454 verified blocks
/// of the reference store).
pub const ALIGN: usize = 512;

/// Work budget of [`Verifier`]: the body checksums of one file may cover at
/// most this many times the file length. Measured on the reference store:
/// 0.88 x (294,782,342 bytes checksummed in a 335,544,320-byte file).
pub const CRC_BUDGET_FACTOR: usize = 4;

/// A block key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// 8-byte key: a page-group id.
    Page(u64),
    /// 16-byte key: a GUID-named system record.
    System([u8; 16]),
}

/// A parsed, verified, decompressed block.
pub struct Block {
    /// File offset of the block header.
    pub offset: usize,
    pub key: Key,
    pub data: Vec<u8>,
}

impl Block {
    /// Key length in bytes (8 or 16), the value the store writes at `+0x10`.
    pub fn kind(&self) -> u32 {
        match self.key {
            Key::Page(_) => 8,
            Key::System(_) => 16,
        }
    }
}

/// Header fields, read but not yet validated.
struct Header {
    crc_header: u32,
    crc_body: u32,
    key_len: usize,
    val_len: usize,
    inflated_len: usize,
    codec: u32,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_header(mm: &[u8], off: usize) -> Option<Header> {
    if off.checked_add(HEADER)? > mm.len() {
        return None;
    }
    let h = &mm[off..off + HEADER];
    Some(Header {
        crc_header: u32_at(h, 0x00),
        crc_body: u32_at(h, 0x04),
        key_len: u32_at(h, 0x10) as usize,
        val_len: u32_at(h, 0x14) as usize,
        inflated_len: u32_at(h, 0x18) as usize,
        codec: u32_at(h, 0x1c),
    })
}

/// The header at `off` and the end offset of its block, when the key length
/// and codec are known, the lengths are sane and the header checksum agrees.
fn checked_header(mm: &[u8], off: usize) -> Option<(Header, usize)> {
    let h = read_header(mm, off)?;

    if (h.key_len != 8 && h.key_len != 16) || h.codec != CODEC_LZ4 {
        return None;
    }

    // Guard against absurd lengths before allocating anything.
    let end = off + HEADER + h.key_len + h.val_len;
    if h.inflated_len == 0 || h.inflated_len > 32 << 20 || end > mm.len() {
        return None;
    }

    if crc32(&mm[off + 4..off + HEADER]) != h.crc_header {
        return None;
    }
    Some((h, end))
}

/// Parse and fully verify the block at `off`.
///
/// Returns `None` unless the key length and codec are known, and the header
/// checksum, the payload checksum and the declared inflated length all agree.
/// Callers can therefore trust the bytes they get back.
pub fn parse(mm: &[u8], off: usize) -> Option<Block> {
    let (h, end) = checked_header(mm, off)?;
    let key_at = off + HEADER;
    let val_at = key_at + h.key_len;
    if crc32(&mm[off + 8..end]) != h.crc_body {
        return None;
    }

    let k = &mm[key_at..val_at];
    let key = if h.key_len == 8 {
        Key::Page(u64::from_le_bytes(k.try_into().unwrap()))
    } else {
        Key::System(k.try_into().unwrap())
    };
    let data = lz::decode_exact(&mm[val_at..end], h.inflated_len)?;
    Some(Block {
        offset: off,
        key,
        data,
    })
}

/// [`parse`] for a pass over a whole file, with a bound on checksum work.
///
/// A body checksum covers whatever length the header claims, so crafted
/// headers that each span to the end of the file would make a pass quadratic.
/// Once the checksummed bytes would exceed [`CRC_BUDGET_FACTOR`] times the
/// file length, further blocks are not verified and are counted in `skipped`.
pub struct Verifier {
    left: usize,
    /// Blocks with a valid header left unverified because the budget was spent.
    pub skipped: usize,
}

impl Verifier {
    pub fn new(mm: &[u8]) -> Verifier {
        Verifier {
            left: mm.len().saturating_mul(CRC_BUDGET_FACTOR),
            skipped: 0,
        }
    }

    /// [`parse`] the block at `off`, or `None` and count it when over budget.
    pub fn parse(&mut self, mm: &[u8], off: usize) -> Option<Block> {
        let (_, end) = checked_header(mm, off)?;
        let cost = end - off - 8;
        if cost > self.left {
            self.skipped += 1;
            return None;
        }
        self.left -= cost;
        parse(mm, off)
    }
}

/// Offsets of every block header in the file.
///
/// Blocks are found by their magic rather than by walking a directory: the
/// store keeps free space and stale regions between live blocks, and the two
/// checksums make a scan safe. Only signatures on an [`ALIGN`] boundary
/// count: a valid block image inside another block's bytes (an uncompressed
/// run of an attachment, say) is data, not a block.
pub fn find_all(mm: &[u8]) -> Vec<usize> {
    let magic = MAGIC.to_le_bytes();
    memchr::memmem::find_iter(mm, &magic)
        .filter(|&m| m >= 8 && (m - 8) % ALIGN == 0)
        .map(|m| m - 8)
        .collect()
}

/// Number of block signatures that [`find_all`] ignores because they are not
/// on an [`ALIGN`] boundary.
pub fn count_unaligned(mm: &[u8]) -> usize {
    let magic = MAGIC.to_le_bytes();
    memchr::memmem::find_iter(mm, &magic)
        .filter(|&m| m < 8 || (m - 8) % ALIGN != 0)
        .count()
}

/// CRC-32 (IEEE, as used by zlib), the checksum the block header uses.
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        t
    });

    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{make_block, make_block_raw, make_store};

    fn value(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn crc32_matches_the_zlib_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn page_key_block_round_trips_the_value() {
        let key = 0x1122_3344_5566_7788u64.to_le_bytes();
        for len in [1, 14, 15, 300] {
            let v = value(len);
            let b = parse(&make_block(&key, &v), 0).unwrap_or_else(|| panic!("len {len}"));
            assert_eq!(b.key, Key::Page(0x1122_3344_5566_7788));
            assert_eq!(b.kind(), 8);
            assert_eq!(b.offset, 0);
            assert_eq!(b.data, v, "len {len}");
        }
    }

    #[test]
    fn system_key_block_parses() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8 + 0xa0);
        let v = value(40);
        let b = parse(&make_block(&key, &v), 0).unwrap();
        assert_eq!(b.key, Key::System(key));
        assert_eq!(b.kind(), 16);
        assert_eq!(b.data, v);
    }

    #[test]
    fn blocks_are_found_and_parsed_inside_a_store() {
        let a = make_block(&1u64.to_le_bytes(), b"first value");
        let b = make_block(&[7u8; 16], b"second value");
        let store = make_store(&[a, b]);
        let offs = find_all(&store);
        assert_eq!(offs.len(), 2);
        assert!(offs.iter().all(|o| o % 512 == 0));
        assert_eq!(parse(&store, offs[0]).unwrap().data, b"first value");
        let second = parse(&store, offs[1]).unwrap();
        assert_eq!(second.offset, offs[1]);
        assert_eq!(second.data, b"second value");
    }

    #[test]
    fn unaligned_block_is_not_found() {
        // A valid block image carried inside another block's value, as a
        // literal LZ4 run: its signature is not on a 512-byte boundary.
        let inner = make_block(&9u64.to_le_bytes(), b"inner value");
        let outer = make_block(&1u64.to_le_bytes(), &inner);
        let store = make_store(&[outer]);
        let at = memchr::memmem::find_iter(&store, &MAGIC.to_le_bytes())
            .map(|m| m - 8)
            .find(|o| o % ALIGN != 0)
            .expect("the inner signature is in the file");
        assert_eq!(parse(&store, at).unwrap().key, Key::Page(9));
        assert_eq!(find_all(&store), [512]);
        assert_eq!(count_unaligned(&store), 1);
        assert_eq!(count_unaligned(&make_store(&[])), 0);
    }

    #[test]
    fn verifier_stops_checksumming_at_the_budget() {
        // Eight ordinary blocks with a 2000-byte value each.
        let big = make_block(&1u64.to_le_bytes(), &value(2000));
        let n = 8;
        let store = make_store(&vec![big; n]);
        let mut v = Verifier::new(&store);
        // Shrink the budget to three bodies instead of building a huge file.
        v.left = 3 * (2000 + 0x30);
        let ok = find_all(&store)
            .into_iter()
            .filter(|&o| v.parse(&store, o).is_some())
            .count();
        assert_eq!((ok, v.skipped), (3, n - 3));
        // The real budget is not hit by a well-formed store.
        let mut v = Verifier::new(&store);
        assert!(find_all(&store)
            .iter()
            .all(|&o| v.parse(&store, o).is_some()));
        assert_eq!(v.skipped, 0);
    }

    #[test]
    fn corrupted_header_crc_is_rejected() {
        let mut b = make_block(&1u64.to_le_bytes(), &value(20));
        b[0] ^= 0xff;
        assert!(parse(&b, 0).is_none());
    }

    #[test]
    fn corrupted_payload_crc_is_rejected() {
        let mut b = make_block(&1u64.to_le_bytes(), &value(20));
        let last = b.len() - 1;
        b[last] ^= 0xff;
        assert!(parse(&b, 0).is_none());
    }

    #[test]
    fn wrong_inflated_length_is_rejected() {
        let v = value(20);
        let b = make_block_raw(&1u64.to_le_bytes(), &v, 8, v.len() as u32 + 1, 4);
        assert!(parse(&b, 0).is_none());
    }

    #[test]
    fn unknown_key_length_is_rejected() {
        let v = value(20);
        let b = make_block_raw(&[1, 2, 3, 4], &v, 4, v.len() as u32, 4);
        assert!(parse(&b, 0).is_none());
    }

    #[test]
    fn unknown_codec_is_rejected() {
        let v = value(20);
        let b = make_block_raw(&1u64.to_le_bytes(), &v, 8, v.len() as u32, 3);
        assert!(parse(&b, 0).is_none());
    }

    #[test]
    fn truncated_block_is_rejected() {
        let b = make_block(&1u64.to_le_bytes(), &value(20));
        assert!(parse(&b[..b.len() - 1], 0).is_none());
        assert!(parse(&b[..HEADER - 1], 0).is_none());
        assert!(parse(&[], 0).is_none());
    }
}
