//! Object identity: one winner per `(type_id, guid)` across all live pages.
//!
//! The same object is stored in several page values (it is copied into each
//! collection that lists it). [`index`] walks the live pages once and keeps,
//! for every schema-typed object, the location of the copy chosen by
//! [`newer`]. It stores locations only, never item bytes.

use std::collections::HashMap;

use crate::item;
use crate::kv::Store;
use crate::props;

/// Where one stored copy of an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    /// Page key of the live value holding the copy.
    pub page: u64,
    /// File offset of that value's live block.
    pub block_offset: usize,
    /// Offset of the item start within the value.
    pub item_offset: usize,
}

/// Result of [`index`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Index {
    winners: HashMap<(u16, [u8; 16]), Located>,
    /// Distinct schema objects (one per `(type_id, guid)`).
    pub schema_objects: usize,
    /// Copies that lost to a newer copy of the same object.
    pub superseded: usize,
    /// Items of a schema type whose `fixed_len` differs from the schema.
    pub fixed_len_mismatch: usize,
    /// Items of a schema type (with the schema's `fixed_len`) whose GUID is
    /// all zero.
    pub no_guid: usize,
    /// Item envelopes left out by the scan budget (`item::scan_budgeted`).
    pub skipped_items: usize,
}

impl Index {
    /// True when the copy at `(page, item_offset)` is the winner for
    /// `(type_id, guid)`.
    pub fn is_winner(&self, type_id: u16, guid: [u8; 16], page: u64, item_offset: usize) -> bool {
        self.winners
            .get(&(type_id, guid))
            .is_some_and(|l| l.page == page && l.item_offset == item_offset)
    }

    /// The winning copy of `(type_id, guid)`, if the object is indexed.
    #[cfg(test)]
    fn get(&self, type_id: u16, guid: [u8; 16]) -> Option<&Located> {
        self.winners.get(&(type_id, guid))
    }
}

/// True when copy `a` is newer than copy `b` of the same object.
///
/// Rule: the copy in the block with the higher file offset wins; within one
/// block, the higher item offset wins. This is an **assumption**: which stored
/// copy is current has not been proven. It mirrors `kv::pick_live` and is
/// isolated here so it can be replaced once the real rule is known.
pub fn newer(a: &Located, b: &Located) -> bool {
    (a.block_offset, a.item_offset) > (b.block_offset, b.item_offset)
}

/// Index the schema-typed objects of every live page value of `store`, in key
/// order. An item is indexed when its type has a schema, its `fixed_len`
/// matches it ([`props::applies`]) and its GUID is not all zero; the other
/// schema-typed items are counted. Items of types without a schema are
/// ignored.
pub fn index(store: &Store, mm: &[u8]) -> Index {
    let mut ix = Index::default();
    for (&key, &block_offset) in &store.pages {
        let Some(data) = store.page_value(mm, key) else {
            continue;
        };
        let scan = item::scan_budgeted(&data);
        ix.skipped_items += scan.skipped;
        for it in &scan.items {
            let Some(schema) = props::schema_for(it.type_id) else {
                continue;
            };
            if !props::applies(schema, it) {
                ix.fixed_len_mismatch += 1;
                continue;
            }
            let Some(guid) = props::guid_of(it) else {
                ix.no_guid += 1;
                continue;
            };
            let here = Located {
                page: key,
                block_offset,
                item_offset: it.offset,
            };
            match ix.winners.get_mut(&(it.type_id, guid)) {
                None => {
                    ix.winners.insert((it.type_id, guid), here);
                }
                Some(best) => {
                    ix.superseded += 1;
                    if newer(&here, best) {
                        *best = here;
                    }
                }
            }
        }
    }
    ix.schema_objects = ix.winners.len();
    ix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::PropValue;
    use crate::testutil::{
        make_block, make_item, make_store, make_typed_item, overlapping_envelopes, page,
    };

    const G1: [u8; 16] = [0x11; 16];
    const G2: [u8; 16] = [0x22; 16];
    const RECIPIENT: u16 = 0xd2;

    fn recipient(guid: [u8; 16], name: &str) -> Vec<u8> {
        make_typed_item(
            "Recipient",
            guid,
            &[("DisplayName", PropValue::Text(name.into()))],
        )
    }

    fn run(blocks: &[Vec<u8>]) -> (Store, Index) {
        let mm = make_store(blocks);
        let store = Store::load(&mm);
        let ix = index(&store, &mm);
        (store, ix)
    }

    #[test]
    fn same_guid_in_two_pages_has_one_winner_from_the_later_block() {
        // The later block holds the lower page key, so key order and file
        // order disagree.
        let (store, ix) = run(&[
            make_block(&page(2), &recipient(G1, "old")),
            make_block(&page(1), &recipient(G1, "new")),
        ]);
        assert!(store.pages[&1] > store.pages[&2]);
        assert_eq!((ix.schema_objects, ix.superseded), (1, 1));
        assert!(ix.is_winner(RECIPIENT, G1, 1, 4));
        assert!(!ix.is_winner(RECIPIENT, G1, 2, 4));
        assert_eq!(
            ix.get(RECIPIENT, G1),
            Some(&Located {
                page: 1,
                block_offset: store.pages[&1],
                item_offset: 4
            })
        );
        assert!(!ix.is_winner(RECIPIENT, G2, 1, 4));
    }

    #[test]
    fn newer_copy_in_a_later_page_replaces_the_first_one_seen() {
        // Key order and file order agree: the index sees the old copy first
        // and has to replace it.
        let (store, ix) = run(&[
            make_block(&page(1), &recipient(G1, "old")),
            make_block(&page(2), &recipient(G1, "new")),
        ]);
        assert!(store.pages[&2] > store.pages[&1]);
        assert_eq!((ix.schema_objects, ix.superseded), (1, 1));
        assert!(ix.is_winner(RECIPIENT, G1, 2, 4));
        assert!(!ix.is_winner(RECIPIENT, G1, 1, 4));
        assert_eq!(
            ix.get(RECIPIENT, G1),
            Some(&Located {
                page: 2,
                block_offset: store.pages[&2],
                item_offset: 4
            })
        );
    }

    #[test]
    fn same_guid_twice_in_one_page_has_one_winner_at_the_higher_offset() {
        let v = [recipient(G1, "old"), recipient(G1, "new")].concat();
        let second = recipient(G1, "old").len() + 4;
        let (_, ix) = run(&[make_block(&page(7), &v)]);
        assert_eq!((ix.schema_objects, ix.superseded), (1, 1));
        assert!(ix.is_winner(RECIPIENT, G1, 7, second));
        assert!(!ix.is_winner(RECIPIENT, G1, 7, 4));
    }

    #[test]
    fn two_guids_of_one_type_are_two_winners() {
        let v = [recipient(G1, "a"), recipient(G2, "b")].concat();
        let second = recipient(G1, "a").len() + 4;
        let (_, ix) = run(&[make_block(&page(7), &v)]);
        assert_eq!((ix.schema_objects, ix.superseded), (2, 0));
        assert!(ix.is_winner(RECIPIENT, G1, 7, 4));
        assert!(ix.is_winner(RECIPIENT, G2, 7, second));
    }

    #[test]
    fn newer_prefers_block_then_item_offset() {
        let at = |block_offset, item_offset| Located {
            page: 0,
            block_offset,
            item_offset,
        };
        assert!(newer(&at(1024, 4), &at(512, 900)));
        assert!(!newer(&at(512, 900), &at(1024, 4)));
        assert!(newer(&at(512, 8), &at(512, 4)));
        assert!(!newer(&at(512, 4), &at(512, 4)));
    }

    #[test]
    fn mismatch_zero_guid_and_unknown_type_are_not_indexed() {
        // A Recipient with the wrong fixed_len.
        let mut wrong = recipient(G1, "a");
        let fl = u16::from_le_bytes([wrong[6], wrong[7]]) - 2;
        wrong[6..8].copy_from_slice(&fl.to_le_bytes());
        // A Recipient with an all-zero GUID.
        let zero = recipient([0; 16], "z");
        // A type without a schema, GUID bytes set.
        let mut unknown = make_item(0xd7, 0x40, &[(0x30, b"x")], &[]);
        unknown[4 + 12..4 + 28].copy_from_slice(&G2);
        assert!(props::schema_for(0xd7).is_none());

        let (_, ix) = run(&[
            make_block(&page(1), &wrong),
            make_block(&page(2), &zero),
            make_block(&page(3), &unknown),
        ]);
        assert_eq!(ix.fixed_len_mismatch, 1);
        assert_eq!(ix.no_guid, 1);
        assert_eq!(
            (ix.schema_objects, ix.superseded, ix.skipped_items),
            (0, 0, 0)
        );
        assert!(ix.get(RECIPIENT, G1).is_none());
        assert!(ix.get(0xd7, G2).is_none());
    }

    #[test]
    fn scan_budget_refusals_are_counted() {
        let v = overlapping_envelopes(1 << 16);
        let expected = item::scan_budgeted(&v).skipped;
        assert!(expected > 0);
        let (_, ix) = run(&[make_block(&page(1), &v)]);
        assert_eq!(ix.skipped_items, expected);
    }

    #[test]
    fn empty_store_has_an_empty_index() {
        let (_, ix) = run(&[]);
        assert_eq!(ix, Index::default());
    }
}
