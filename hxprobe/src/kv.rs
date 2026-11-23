//! The store as a key-value map: every key resolved to one live block.
//!
//! A key can be written several times; older copies stay on disk until their
//! space is reused. [`Store`] keeps only the file offset of the live revision
//! of each key, so loading costs one verification pass and no retained data.
//! Values are decompressed on demand, one at a time.

use std::collections::BTreeMap;

use crate::block::{self, Key};

/// Counters from one [`Store::load`] pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Block signatures found in the file.
    pub blocks: usize,
    /// Signatures whose block failed verification.
    pub failed: usize,
    /// Blocks left unverified because the checksum work budget of
    /// [`block::Verifier`] was spent (0 for any store Outlook wrote).
    pub over_budget: usize,
    /// Distinct 8-byte (page-group) keys.
    pub page_keys: usize,
    /// Distinct 16-byte (system) keys.
    pub system_keys: usize,
    /// Verified blocks that are not the live revision of their key.
    pub stale_revisions: usize,
}

/// Key -> file offset of the live block.
pub struct Store {
    pub pages: BTreeMap<u64, usize>,
    pub system: BTreeMap<[u8; 16], usize>,
    pub stats: Stats,
}

/// Choose the live revision among the file offsets of one key's blocks.
///
/// Rule: the highest file offset wins. This is **unverified**: the store's
/// index has not been decoded, so it is an assumption, isolated here so it can
/// be replaced once the real rule is known. `offsets` must be non-empty.
pub fn pick_live(offsets: &[usize]) -> usize {
    *offsets
        .iter()
        .max()
        .expect("pick_live needs at least one offset")
}

fn resolve<K: Ord>(revs: BTreeMap<K, Vec<usize>>, stale: &mut usize) -> BTreeMap<K, usize> {
    revs.into_iter()
        .map(|(k, offs)| {
            *stale += offs.len() - 1;
            (k, pick_live(&offs))
        })
        .collect()
}

impl Store {
    /// Scan and verify every block in `mm`, recording one offset per key.
    pub fn load(mm: &[u8]) -> Store {
        let offs = block::find_all(mm);
        let mut stats = Stats {
            blocks: offs.len(),
            ..Stats::default()
        };
        let mut pages: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        let mut system: BTreeMap<[u8; 16], Vec<usize>> = BTreeMap::new();

        let mut verifier = block::Verifier::new(mm);
        for o in offs {
            // `parse` decompresses to verify the inflated length; the data is
            // dropped right away so only offsets are kept.
            match verifier.parse(mm, o).map(|b| b.key) {
                Some(Key::Page(k)) => pages.entry(k).or_default().push(o),
                Some(Key::System(k)) => system.entry(k).or_default().push(o),
                None => stats.failed += 1,
            }
        }
        stats.over_budget = verifier.skipped;
        stats.failed -= verifier.skipped;

        let pages = resolve(pages, &mut stats.stale_revisions);
        let system = resolve(system, &mut stats.stale_revisions);
        stats.page_keys = pages.len();
        stats.system_keys = system.len();
        Store {
            pages,
            system,
            stats,
        }
    }

    /// Decompressed live value of page key `key`.
    pub fn page_value(&self, mm: &[u8], key: u64) -> Option<Vec<u8>> {
        block::parse(mm, *self.pages.get(&key)?).map(|b| b.data)
    }

    /// Decompressed live value of system key `key`.
    pub fn system_value(&self, mm: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
        block::parse(mm, *self.system.get(key)?).map(|b| b.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{make_block, make_store, overlapping_blocks, page};

    #[test]
    fn highest_offset_is_live() {
        assert_eq!(pick_live(&[512]), 512);
        assert_eq!(pick_live(&[1024, 4096, 2048]), 4096);
    }

    #[test]
    fn later_revision_wins_and_older_is_stale() {
        let mm = make_store(&[
            make_block(&page(7), b"old revision"),
            make_block(&page(9), b"other key"),
            make_block(&page(7), b"new revision"),
        ]);
        let s = Store::load(&mm);
        assert_eq!(s.stats.blocks, 3);
        assert_eq!(s.stats.failed, 0);
        assert_eq!(s.stats.page_keys, 2);
        assert_eq!(s.stats.stale_revisions, 1);
        assert_eq!(s.page_value(&mm, 7).unwrap(), b"new revision");
        assert_eq!(s.page_value(&mm, 9).unwrap(), b"other key");
    }

    #[test]
    fn sixteen_byte_key_lands_in_system() {
        let guid = [0x5au8; 16];
        let mm = make_store(&[make_block(&guid, b"system record")]);
        let s = Store::load(&mm);
        assert!(s.pages.is_empty());
        assert_eq!(s.stats.system_keys, 1);
        assert_eq!(s.stats.page_keys, 0);
        assert_eq!(s.system_value(&mm, &guid).unwrap(), b"system record");
        assert!(s.system_value(&mm, &[0u8; 16]).is_none());
    }

    #[test]
    fn corrupt_block_is_counted_and_others_load() {
        let mut bad = make_block(&page(2), b"corrupt me");
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        let mm = make_store(&[
            make_block(&page(1), b"good one"),
            bad,
            make_block(&page(3), b"good three"),
        ]);
        let s = Store::load(&mm);
        assert_eq!(s.stats.blocks, 3);
        assert_eq!(s.stats.failed, 1);
        assert_eq!(s.stats.page_keys, 2);
        assert!(s.page_value(&mm, 2).is_none());
        assert_eq!(s.page_value(&mm, 3).unwrap(), b"good three");
    }

    #[test]
    fn load_stops_checksumming_at_the_budget() {
        // 64 headers that each claim the rest of a 33,280-byte file. The
        // budget of 4 x the file length covers the first four bodies and one
        // short body near the end; the other 59 are not checksummed.
        let mm = overlapping_blocks(64);
        let s = Store::load(&mm).stats;
        assert_eq!(block::CRC_BUDGET_FACTOR, 4);
        assert_eq!((s.blocks, s.failed, s.over_budget), (64, 5, 59));
        let verified = s.page_keys + s.system_keys + s.stale_revisions;
        assert_eq!(s.failed + s.over_budget + verified, s.blocks);

        // A real block among them is still verified and counted once.
        let mut mm = overlapping_blocks(64);
        let good = make_block(&page(7), b"value");
        mm[512..512 + good.len()].copy_from_slice(&good);
        let s = Store::load(&mm);
        assert_eq!(s.page_value(&mm, 7).unwrap(), b"value");
        assert!(s.stats.over_budget > 0);
        let verified = s.stats.page_keys + s.stats.system_keys + s.stats.stale_revisions;
        assert_eq!(verified, 1);
        assert_eq!(
            s.stats.failed + s.stats.over_budget + verified,
            s.stats.blocks
        );
    }

    #[test]
    fn header_only_store_has_no_keys() {
        let mm = make_store(&[]);
        let s = Store::load(&mm);
        assert_eq!(s.stats, Stats::default());
        assert!(s.pages.is_empty() && s.system.is_empty());
    }

    #[test]
    fn missing_key_is_none() {
        let mm = make_store(&[make_block(&page(1), b"value")]);
        let s = Store::load(&mm);
        assert!(s.page_value(&mm, 42).is_none());
    }
}
