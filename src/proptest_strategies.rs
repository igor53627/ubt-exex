//! Proptest strategies for UBT property-based testing.

use alloy_primitives::B256;
use proptest::prelude::*;
use ubt::{Stem, TreeKey, STEM_LEN};

pub const MAX_STEMS: usize = 4;
pub const MAX_SUBINDICES: u8 = 8;
pub const MAX_BLOCKS: usize = 5;
pub const MAX_ENTRIES_PER_BLOCK: usize = 10;

#[derive(Clone, Debug)]
pub struct Entry {
    pub stem: Stem,
    pub subindex: u8,
    pub value: B256,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub entries: Vec<Entry>,
}

pub fn arb_stem() -> impl Strategy<Value = Stem> {
    (0u16..256u16).prop_map(|id| {
        let mut bytes = [0u8; STEM_LEN];
        bytes[0] = (id & 0xFF) as u8;
        bytes[1] = (id >> 8) as u8;
        Stem::from(bytes)
    })
}

pub fn arb_value() -> impl Strategy<Value = B256> {
    prop_oneof![
        3 => any::<[u8; 32]>().prop_map(B256::from),
        1 => Just(B256::ZERO),
        1 => prop::array::uniform32(0u8..16).prop_map(B256::from),
    ]
}

pub fn arb_entry(stems: Vec<Stem>) -> impl Strategy<Value = Entry> {
    let stem_strategy = if stems.is_empty() {
        arb_stem().boxed()
    } else {
        prop::sample::select(stems).boxed()
    };

    (stem_strategy, 0..=MAX_SUBINDICES, arb_value()).prop_map(|(stem, subindex, value)| Entry {
        stem,
        subindex,
        value,
    })
}

pub fn arb_block() -> impl Strategy<Value = Block> {
    prop::collection::vec(arb_stem(), 1..=MAX_STEMS).prop_flat_map(|stems| {
        prop::collection::vec(arb_entry(stems), 1..=MAX_ENTRIES_PER_BLOCK)
            .prop_map(|entries| Block { entries })
    })
}

pub fn arb_blocks() -> impl Strategy<Value = Vec<Block>> {
    prop::collection::vec(arb_stem(), 1..=MAX_STEMS).prop_flat_map(|global_stems| {
        prop::collection::vec(
            prop::collection::vec(arb_entry(global_stems.clone()), 1..=MAX_ENTRIES_PER_BLOCK)
                .prop_map(|entries| Block { entries }),
            1..=MAX_BLOCKS,
        )
    })
}

pub fn to_tree_entries(block: &Block) -> Vec<(TreeKey, B256)> {
    block
        .entries
        .iter()
        .map(|e| {
            let key = TreeKey {
                stem: e.stem,
                subindex: e.subindex,
            };
            (key, e.value)
        })
        .collect()
}

#[allow(dead_code)]
pub fn to_tree_entries_flat(blocks: &[Block]) -> Vec<(TreeKey, B256)> {
    blocks.iter().flat_map(to_tree_entries).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::proptest;

    proptest! {
        #[test]
        fn test_arb_stem_generates_valid_stems(stem in arb_stem()) {
            assert_eq!(stem.as_bytes().len(), STEM_LEN);
        }

        #[test]
        fn test_arb_value_generates_valid_values(value in arb_value()) {
            assert_eq!(value.len(), 32);
        }

        #[test]
        fn test_arb_block_generates_non_empty_blocks(block in arb_block()) {
            assert!(!block.entries.is_empty());
            assert!(block.entries.len() <= MAX_ENTRIES_PER_BLOCK);
        }

        #[test]
        fn test_arb_blocks_generates_valid_sequence(blocks in arb_blocks()) {
            assert!(!blocks.is_empty());
            assert!(blocks.len() <= MAX_BLOCKS);
        }

        #[test]
        fn test_to_tree_entries_preserves_count(block in arb_block()) {
            let entries = to_tree_entries(&block);
            assert_eq!(entries.len(), block.entries.len());
        }
    }
}
