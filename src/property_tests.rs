//! Property-based tests for UbtExEx.
//!
//! Uses proptest to verify invariants about state transitions, revert logic,
//! and determinism of the UBT implementation.

use alloy_primitives::B256;
use proptest::prelude::*;
use std::collections::{HashMap, HashSet};
use ubt::TreeKey;

use crate::proptest_strategies::{arb_block, arb_blocks, to_tree_entries};
use crate::ubt_exex::tests::TestHarness;

fn make_block_hash(n: u64) -> B256 {
    B256::from_slice(&{
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&n.to_le_bytes());
        bytes
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_delta_apply_then_revert_restores_state(block in arb_block()) {
        let mut harness = TestHarness::new();
        let entries_before = harness.snapshot_entries();
        let root_before = harness.snapshot_root();

        let entries = to_tree_entries(&block);
        harness.apply_entries_block(1, make_block_hash(1), entries);

        let deltas = harness.exex.db.load_block_deltas(1).unwrap();
        harness.exex.apply_deltas_reverse(1, &deltas).unwrap();

        let dirty: Vec<_> = harness.exex.dirty_stems.drain().collect();
        harness.exex.db.batch_update_stems(&dirty).unwrap();

        let entries_after = harness.snapshot_entries();
        let root_after = harness.snapshot_root();

        prop_assert_eq!(entries_before, entries_after, "entries should match after revert");
        prop_assert_eq!(root_before, root_after, "root should match after revert");
    }

    #[test]
    fn prop_revert_then_replay_same_final_state(blocks in arb_blocks()) {
        if blocks.len() < 2 {
            return Ok(());
        }

        let mut harness = TestHarness::new();

        for (i, block) in blocks[..blocks.len() - 1].iter().enumerate() {
            let entries = to_tree_entries(block);
            harness.apply_entries_block((i + 1) as u64, make_block_hash((i + 1) as u64), entries);
        }

        let state_before_bn = harness.snapshot_entries();
        let root_before_bn = harness.snapshot_root();

        let bn = blocks.len() as u64;
        let last_block = &blocks[blocks.len() - 1];
        let last_entries = to_tree_entries(last_block);
        harness.apply_entries_block(bn, make_block_hash(bn), last_entries.clone());

        let state_after_bn = harness.snapshot_entries();
        let root_after_bn = harness.snapshot_root();

        let deltas = harness.exex.db.load_block_deltas(bn).unwrap();
        harness.exex.apply_deltas_reverse(bn, &deltas).unwrap();

        let dirty: Vec<_> = harness.exex.dirty_stems.drain().collect();
        harness.exex.db.batch_update_stems(&dirty).unwrap();

        let state_reverted = harness.snapshot_entries();
        let root_reverted = harness.snapshot_root();

        prop_assert_eq!(state_before_bn, state_reverted, "state should match before Bn after revert");
        prop_assert_eq!(root_before_bn, root_reverted, "root should match before Bn after revert");

        harness.apply_entries_block(bn, make_block_hash(bn), last_entries);

        let state_replayed = harness.snapshot_entries();
        let root_replayed = harness.snapshot_root();

        prop_assert_eq!(state_after_bn, state_replayed, "state should match after replay");
        prop_assert_eq!(root_after_bn, root_replayed, "root should match after replay");
    }

    #[test]
    fn prop_root_determinism_single_block(block in arb_block()) {
        let entries = to_tree_entries(&block);
        if entries.is_empty() {
            return Ok(());
        }

        let mut deduped: HashMap<TreeKey, B256> = HashMap::new();
        for (key, value) in &entries {
            deduped.insert(*key, *value);
        }
        let unique_entries: Vec<_> = deduped.into_iter().collect();

        if unique_entries.is_empty() {
            return Ok(());
        }

        let mut harness1 = TestHarness::new();
        let root1 = harness1.apply_entries_block(1, make_block_hash(1), unique_entries.clone());

        let mut harness2 = TestHarness::new();
        let mut reversed_entries = unique_entries.clone();
        reversed_entries.reverse();
        let root2 = harness2.apply_entries_block(1, make_block_hash(1), reversed_entries);

        let mut harness3 = TestHarness::new();
        let mut sorted_entries = unique_entries.clone();
        sorted_entries.sort_by(|a, b| a.0.to_bytes().cmp(&b.0.to_bytes()));
        let root3 = harness3.apply_entries_block(1, make_block_hash(1), sorted_entries);

        prop_assert_eq!(root1, root2, "reversed order should produce same root");
        prop_assert_eq!(root1, root3, "sorted order should produce same root");
    }

    #[test]
    fn prop_overlay_mdbx_matches_model(blocks in arb_blocks()) {
        let mut harness = TestHarness::new();
        let mut model: HashMap<TreeKey, B256> = HashMap::new();

        for (i, block) in blocks.iter().enumerate() {
            let entries = to_tree_entries(block);

            for (key, value) in &entries {
                model.insert(*key, *value);
            }

            harness.apply_entries_block((i + 1) as u64, make_block_hash((i + 1) as u64), entries.clone());

            for (key, expected_value) in &model {
                let actual = harness.exex.get_value(key).unwrap().unwrap_or(B256::ZERO);
                prop_assert_eq!(
                    *expected_value,
                    actual,
                    "value mismatch for key {:?} at block {}",
                    key,
                    i + 1
                );
            }
        }
    }

    #[test]
    fn prop_stem_count_monotone(blocks in arb_blocks()) {
        let mut harness = TestHarness::new();
        let mut seen_stems: HashSet<ubt::Stem> = HashSet::new();
        let mut prev_stem_count = 0usize;

        for (i, block) in blocks.iter().enumerate() {
            let entries = to_tree_entries(block);

            for (key, _) in &entries {
                seen_stems.insert(key.stem);
            }

            harness.apply_entries_block((i + 1) as u64, make_block_hash((i + 1) as u64), entries);

            let current_stem_count = harness.exex.stem_count();

            prop_assert!(
                current_stem_count >= prev_stem_count,
                "stem_count should be monotonically increasing: {} < {}",
                current_stem_count,
                prev_stem_count
            );
            prop_assert_eq!(
                current_stem_count,
                seen_stems.len(),
                "stem_count should equal seen_stems.len()"
            );

            prev_stem_count = current_stem_count;
        }
    }

    #[test]
    fn prop_multi_block_reorg(blocks in arb_blocks()) {
        if blocks.len() < 3 {
            return Ok(());
        }

        let mut harness = TestHarness::new();
        let mut model: HashMap<TreeKey, B256> = HashMap::new();

        for (i, block) in blocks.iter().enumerate() {
            let entries = to_tree_entries(block);
            for (key, value) in &entries {
                model.insert(*key, *value);
            }
            harness.apply_entries_block((i + 1) as u64, make_block_hash((i + 1) as u64), entries);
        }

        let reorg_depth = 2;
        let reorg_start = blocks.len() as u64 - reorg_depth + 1;

        for bn in (reorg_start..=blocks.len() as u64).rev() {
            let deltas = harness.exex.db.load_block_deltas(bn).unwrap();
            for (stem, subindex, old_value) in deltas.iter().rev() {
                let key = TreeKey { stem: *stem, subindex: *subindex };
                model.insert(key, *old_value);
            }
            harness.exex.apply_deltas_reverse(bn, &deltas).unwrap();
        }

        let dirty: Vec<_> = harness.exex.dirty_stems.drain().collect();
        harness.exex.db.batch_update_stems(&dirty).unwrap();

        for (key, expected) in &model {
            let actual = harness.exex.get_value(key).unwrap().unwrap_or(B256::ZERO);
            prop_assert_eq!(
                *expected,
                actual,
                "mismatch after multi-block reorg for key {:?}",
                key
            );
        }

        let db_entries = harness.snapshot_entries();
        let mut model_entries: Vec<_> = model
            .iter()
            .filter(|(_, v)| **v != B256::ZERO)
            .map(|(k, v)| (*k, *v))
            .collect();
        model_entries.sort_by(|a, b| a.0.to_bytes().cmp(&b.0.to_bytes()));

        let mut db_entries_sorted = db_entries.clone();
        db_entries_sorted.sort_by(|a, b| a.0.to_bytes().cmp(&b.0.to_bytes()));

        prop_assert_eq!(
            model_entries.len(),
            db_entries_sorted.len(),
            "entry count should match after reorg"
        );
    }
}
