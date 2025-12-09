//! UBT ExEx implementation.
//!
//! This module contains the core Execution Extension that maintains a Unified Binary Tree
//! tracking all Ethereum state changes in real-time.
//!
//! # Architecture
//!
//! The ExEx receives block notifications from reth and:
//! 1. Extracts state changes from the `BundleState`
//! 2. Converts changes to UBT key-value pairs
//! 3. Updates the dirty overlay (in-memory)
//! 4. Periodically flushes to MDBX for durability
//! 5. Computes root hash from MDBX + overlay using streaming
//!
//! # Memory Model
//!
//! State is stored in MDBX (canonical) with a small dirty overlay in memory.
//! The full tree is NOT loaded at startup - only the overlay accumulates changes
//! between flushes. This reduces memory from ~80GB+ to <1GB for large state.
//!
//! When mutating a stem, the overlay is seeded from MDBX if not already present.
//! This ensures all subindex values are preserved when updating a single subindex.
//!
//! # Reorg Handling
//!
//! State deltas are stored per-block, allowing reverts to restore previous values.
//! Deltas are pruned after `delta_retention` blocks to bound storage growth.

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use futures::TryStreamExt;
use reth_ethereum::exex::{ExExContext, ExExEvent, ExExHead, ExExNotification};
use reth_exex::ExExNotificationsStream;
use reth_execution_types::Chain;
use reth_node_api::FullNodeComponents;
use reth_primitives_traits::{AlloyBlockHeader as _, NodePrimitives};
use std::{collections::HashMap, time::Instant};
use tracing::{debug, info, warn};
use ubt::{
    chunkify_code, get_basic_data_key, get_code_chunk_key, get_code_hash_key, get_storage_slot_key,
    BasicDataLeaf, Blake3Hasher, Stem, StemNode, StreamingTreeBuilder, TreeKey,
};

use crate::config::UbtConfig;
use crate::error::Result;
use crate::persistence::{UbtDatabase, UbtHead};

const UBT_DATA_DIR: &str = "ubt";

pub struct UbtExEx {
    pub(crate) db: UbtDatabase,
    last_block: u64,
    last_hash: B256,
    last_root: B256,
    pub(crate) pending_entries: Vec<(TreeKey, B256)>,
    pub(crate) dirty_stems: HashMap<Stem, StemNode>,
    flush_interval: u64,
    delta_retention: u64,
    last_persisted_block: u64,
    last_persisted_hash: B256,
    stem_count: usize,
}

impl UbtExEx {
    /// Create a new UBT ExEx instance with the given configuration.
    ///
    /// Note: Does NOT load the full tree into memory. State is read from MDBX
    /// on demand with a dirty overlay for pending changes.
    pub fn new(config: &UbtConfig) -> Result<Self> {
        let data_dir = config.get_data_dir();
        let ubt_dir = data_dir.join(UBT_DATA_DIR);
        let db = UbtDatabase::open(&ubt_dir)?;
        let flush_interval = config.get_flush_interval();
        let delta_retention = config.get_delta_retention();

        let (last_block, last_hash, last_root, last_persisted_block, last_persisted_hash, stem_count) =
            if let Some(head) = db.load_head()? {
                info!(
                    block = head.block_number,
                    root = %head.root,
                    stems = head.stem_count,
                    "Resuming UBT state from MDBX (not loading full tree)"
                );

                info!("Verifying UBT root via streaming; this may take a while on large state");
                let computed = compute_root_streaming(&db)?;
                if computed == head.root {
                    info!("Streaming root verification passed");
                } else {
                    return Err(crate::error::UbtError::RootVerificationFailed {
                        expected: format!("{}", head.root),
                        computed: format!("{}", computed),
                    });
                }

                (
                    head.block_number,
                    head.block_hash,
                    head.root,
                    head.block_number,
                    head.block_hash,
                    head.stem_count,
                )
            } else {
                info!("Starting fresh UBT state");
                (0, B256::ZERO, B256::ZERO, 0, B256::ZERO, 0)
            };

        info!(
            flush_interval = flush_interval,
            delta_retention = delta_retention,
            "UBT flush interval configured"
        );

        Ok(Self {
            db,
            last_block,
            last_hash,
            last_root,
            pending_entries: Vec::new(),
            dirty_stems: HashMap::new(),
            flush_interval,
            delta_retention,
            last_persisted_block,
            last_persisted_hash,
            stem_count,
        })
    }

    /// Get the current stem count.
    #[allow(dead_code)]
    pub fn stem_count(&self) -> usize {
        self.stem_count
    }

    /// Get the last persisted head for ExEx resumption.
    ///
    /// Returns `None` if no blocks have been persisted yet (fresh start).
    /// Note: Block 0 (genesis) is treated as "no head" - this assumes genesis
    /// is not persisted directly but rather processed via backfill.
    pub fn get_head(&self) -> Option<ExExHead> {
        if self.last_persisted_block == 0 {
            None
        } else {
            Some(ExExHead::new(BlockNumHash::new(
                self.last_persisted_block,
                self.last_persisted_hash,
            )))
        }
    }

    pub fn process_chain<N: NodePrimitives>(&mut self, chain: &Chain<N>) -> Result<()> {
        let execution_outcome = chain.execution_outcome();
        let bundle = execution_outcome.state();

        for (address, account) in bundle.state() {
            let address = *address;

            if let Some(info) = &account.info {
                let code_size = info.code.as_ref().map(|c| c.len()).unwrap_or(0) as u32;

                let basic_data =
                    BasicDataLeaf::new(info.nonce, balance_to_u128(info.balance), code_size);
                let key = get_basic_data_key(&address);
                self.pending_entries.push((key, basic_data.encode()));

                if info.code_hash != B256::ZERO && info.code_hash != KECCAK_EMPTY {
                    let code_hash_key = get_code_hash_key(&address);
                    self.pending_entries.push((code_hash_key, info.code_hash));

                    if let Some(code) = &info.code {
                        let chunks = chunkify_code(code.original_byte_slice());
                        for (i, chunk) in chunks.iter().enumerate() {
                            let chunk_key = get_code_chunk_key(&address, i as u64);
                            self.pending_entries.push((chunk_key, chunk.encode()));
                        }
                    }
                }
            }

            for (slot, value) in &account.storage {
                let slot_bytes = u256_to_b256((*slot).into());
                let value_bytes = u256_to_b256(value.present_value);
                let storage_key = get_storage_slot_key(&address, &slot_bytes.0);
                self.pending_entries.push((storage_key, value_bytes));
            }
        }

        Ok(())
    }

    /// Commit pending entries to the UBT state for the given block.
    ///
    /// Returns the UBT root hash. Note: the returned root is only updated on flush
    /// (every `flush_interval` blocks). Between flushes, returns the last persisted root.
    /// This is a performance optimization - the true tip root could be computed on demand
    /// but would require merging dirty overlay with MDBX for every block.
    pub fn commit(&mut self, block_number: u64, block_hash: B256) -> Result<B256> {
        let entries = std::mem::take(&mut self.pending_entries);
        let entry_count = entries.len();

        let mut deltas: Vec<(Stem, u8, B256)> = Vec::new();
        let mut seen_new_stems: std::collections::HashSet<Stem> = std::collections::HashSet::new();

        for (key, value) in &entries {
            if !self.dirty_stems.contains_key(&key.stem) {
                if let Some(existing) = self.db.load_stem(&key.stem)? {
                    self.dirty_stems.insert(key.stem, existing);
                } else {
                    seen_new_stems.insert(key.stem);
                    self.dirty_stems.insert(key.stem, StemNode::new(key.stem));
                }
            }

            let stem_node = self.dirty_stems.get_mut(&key.stem).expect("just inserted");
            let old_value = stem_node.get_value(key.subindex).unwrap_or(B256::ZERO);

            if old_value != *value {
                deltas.push((key.stem, key.subindex, old_value));
            }

            stem_node.set_value(key.subindex, *value);
        }

        self.stem_count += seen_new_stems.len();

        if !deltas.is_empty() {
            self.db.save_block_deltas(block_number, &deltas)?;
        }

        self.last_block = block_number;
        self.last_hash = block_hash;

        let should_flush = block_number <= self.last_persisted_block
            || (block_number - self.last_persisted_block) >= self.flush_interval;

        if should_flush {
            let persist_start = Instant::now();
            let dirty: Vec<_> = self.dirty_stems.drain().collect();
            let dirty_count = dirty.len();
            if !dirty.is_empty() {
                self.db.batch_update_stems(&dirty)?;
            }

            let root_start = Instant::now();
            let root = self.compute_root_streaming()?;
            crate::metrics::record_root_computation(root_start.elapsed().as_secs_f64());

            let head = UbtHead {
                block_number,
                block_hash,
                root,
                stem_count: self.stem_count,
            };
            self.db.save_head(&head)?;
            crate::metrics::record_persistence(persist_start.elapsed().as_secs_f64(), dirty_count);
            crate::metrics::record_dirty_stems(0);

            self.last_persisted_block = block_number;
            self.last_persisted_hash = block_hash;
            self.last_root = root;

            info!(
                block = block_number,
                entries = entry_count,
                stems = self.stem_count,
                dirty_stems = dirty_count,
                root = %root,
                "UBT updated and flushed to MDBX"
            );

            if block_number > self.delta_retention {
                let prune_before = block_number - self.delta_retention;
                match self.db.prune_deltas_before(prune_before) {
                    Ok(count) if count > 0 => {
                        debug!(pruned = count, before_block = prune_before, "Pruned old deltas");
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to prune old deltas");
                    }
                    _ => {}
                }
            }

            crate::metrics::record_block_processed(block_number, entry_count, self.stem_count);
            Ok(root)
        } else {
            crate::metrics::record_dirty_stems(self.dirty_stems.len());
            debug!(
                block = block_number,
                entries = entry_count,
                pending_stems = self.dirty_stems.len(),
                blocks_until_flush = self.flush_interval - (block_number - self.last_persisted_block),
                "UBT updated in-memory (pending flush)"
            );

            crate::metrics::record_block_processed(block_number, entry_count, self.stem_count);
            Ok(self.last_root)
        }
    }

    /// Revert the UBT state for the given chain of blocks.
    ///
    /// Applies stored deltas in reverse order to restore previous values.
    ///
    /// Note: `stem_count` is not decremented during reverts, so it may be slightly
    /// inflated after reorgs. This is a known limitation - accurate stem counting
    /// would require scanning MDBX which is expensive. The count is reset on restart
    /// from the persisted head.
    pub fn revert(&mut self, chain: &Chain<impl NodePrimitives>) -> Result<()> {
        let blocks = chain.blocks();
        let mut block_numbers: Vec<u64> = blocks.keys().copied().collect();
        block_numbers.sort();
        block_numbers.reverse();

        let mut total_reverted = 0usize;
        let mut reverted_persisted = false;

        for block_number in &block_numbers {
            let deltas = self.db.load_block_deltas(*block_number)?;

            if deltas.is_empty() {
                warn!(
                    block = *block_number,
                    retention = self.delta_retention,
                    "No deltas found while reverting block; reorg may exceed delta_retention"
                );
            }

            self.apply_deltas_reverse(*block_number, &deltas)?;
            total_reverted += deltas.len();

            if *block_number > self.last_persisted_block {
                self.db.delete_block_deltas(*block_number)?;
            }
        }

        if let Some((&first_reverted_num, first_reverted_block)) = blocks.iter().min_by_key(|(num, _)| *num) {
            if first_reverted_num > 0 {
                self.last_block = first_reverted_num - 1;
                self.last_hash = first_reverted_block.header().parent_hash();
            } else {
                self.last_block = 0;
                self.last_hash = B256::ZERO;
            }
            reverted_persisted = blocks.keys().any(|&b| b <= self.last_persisted_block);
        }

        if reverted_persisted {
            let dirty: Vec<_> = self.dirty_stems.drain().collect();
            if !dirty.is_empty() {
                self.db.batch_update_stems(&dirty)?;
            }

            let root = self.compute_root_streaming()?;
            let head = UbtHead {
                block_number: self.last_block,
                block_hash: self.last_hash,
                root,
                stem_count: self.stem_count,
            };
            self.db.save_head(&head)?;

            self.last_persisted_block = self.last_block;
            self.last_persisted_hash = self.last_hash;
            self.last_root = root;
        }

        info!(
            blocks = block_numbers.len(),
            entries = total_reverted,
            new_head = self.last_block,
            reverted_persisted = reverted_persisted,
            "UBT reverted blocks"
        );

        crate::metrics::record_revert(block_numbers.len(), total_reverted);

        Ok(())
    }

    /// Get a stem node, checking dirty overlay first, then MDBX.
    /// Used for proof generation without requiring full tree in memory.
    ///
    /// Note: Returns a clone of the StemNode. If profiling shows this is a hot path,
    /// consider using Cow<'_, StemNode> or a closure-based API to avoid cloning.
    #[allow(dead_code)]
    pub fn get_stem(&self, stem: &Stem) -> Result<Option<StemNode>> {
        if let Some(node) = self.dirty_stems.get(stem) {
            return Ok(Some(node.clone()));
        }
        self.db.load_stem(stem)
    }

    /// Get a specific value by TreeKey, checking overlay then MDBX.
    #[allow(dead_code)]
    pub fn get_value(&self, key: &TreeKey) -> Result<Option<B256>> {
        if let Some(node) = self.dirty_stems.get(&key.stem) {
            if let Some(value) = node.get_value(key.subindex) {
                return Ok(Some(value));
            }
        }
        self.db.load_value(key)
    }

    /// Gracefully shutdown, flushing all pending state to MDBX.
    pub fn shutdown(&mut self) -> Result<()> {
        info!("UBT ExEx shutting down, flushing pending state...");

        let dirty: Vec<_> = self.dirty_stems.drain().collect();
        if !dirty.is_empty() {
            info!(stems = dirty.len(), "Flushing dirty stems");
            self.db.batch_update_stems(&dirty)?;
        }

        let root = self.compute_root_streaming()?;
        let head = UbtHead {
            block_number: self.last_block,
            block_hash: self.last_hash,
            root,
            stem_count: self.stem_count,
        };
        self.db.save_head(&head)?;

        info!(
            block = self.last_block,
            root = %root,
            "UBT ExEx shutdown complete"
        );

        Ok(())
    }

    /// Compute root hash from MDBX entries using streaming builder with parallel hashing.
    ///
    /// This reads all entries from MDBX and computes the root without
    /// keeping the full tree in memory. Uses rayon for parallel stem hashing.
    /// Note: still creates a Vec of all entries, so memory spikes during computation.
    pub(crate) fn compute_root_streaming(&self) -> Result<B256> {
        let entries = self.db.iter_entries_sorted()?;
        let root = StreamingTreeBuilder::<Blake3Hasher>::new().build_root_hash_parallel(entries);
        Ok(root)
    }

    /// Apply deltas in reverse order to revert state changes.
    ///
    /// This is the core logic shared by revert operations. Given a list of deltas
    /// (stem, subindex, old_value), applies them in reverse order to restore
    /// previous values.
    #[allow(dead_code)]
    pub(crate) fn apply_deltas_reverse(
        &mut self,
        block_number: u64,
        deltas: &[(Stem, u8, B256)],
    ) -> Result<()> {
        for (stem, subindex, old_value) in deltas.iter().rev() {
            if !self.dirty_stems.contains_key(stem) {
                if let Some(existing) = self.db.load_stem(stem)? {
                    self.dirty_stems.insert(*stem, existing);
                } else {
                    self.dirty_stems.insert(*stem, StemNode::new(*stem));
                }
            }
            let stem_node = self.dirty_stems.get_mut(stem).expect("just inserted");
            stem_node.set_value(*subindex, *old_value);
        }

        if block_number > self.last_persisted_block {
            self.db.delete_block_deltas(block_number)?;
        }

        Ok(())
    }
}

/// Compute root hash from MDBX using streaming builder with parallel hashing.
fn compute_root_streaming(db: &UbtDatabase) -> Result<B256> {
    let entries = db.iter_entries_sorted()?;
    let root = StreamingTreeBuilder::<Blake3Hasher>::new().build_root_hash_parallel(entries);
    Ok(root)
}

const KECCAK_EMPTY: B256 = B256::new([
    0xc5, 0xd2, 0x46, 0x01, 0x86, 0xf7, 0x23, 0x3c, 0x92, 0x7e, 0x7d, 0xb2, 0xdc, 0xc7, 0x03, 0xc0,
    0xe5, 0x00, 0xb6, 0x53, 0xca, 0x82, 0x27, 0x3b, 0x7b, 0xfa, 0xd8, 0x04, 0x5d, 0x85, 0xa4, 0x70,
]);

fn balance_to_u128(balance: U256) -> u128 {
    if balance > U256::from(u128::MAX) {
        u128::MAX
    } else {
        balance.to::<u128>()
    }
}

fn u256_to_b256(value: U256) -> B256 {
    B256::from(value.to_be_bytes::<32>())
}

/// Main entry point for the UBT ExEx.
///
/// Configuration precedence: environment variables > defaults.
///
/// Note: CLI args are defined in `UbtConfig` with clap derives but are not yet
/// wired through reth's CLI extension system. For now, use environment variables:
/// - `RETH_DATA_DIR` - base data directory
/// - `UBT_FLUSH_INTERVAL` - blocks between MDBX flushes
/// - `UBT_DELTA_RETENTION` - blocks to retain deltas for reorgs
pub async fn ubt_exex<Node: FullNodeComponents>(mut ctx: ExExContext<Node>) -> eyre::Result<()> {
    let config = UbtConfig::default();

    if config.disabled {
        info!("UBT ExEx disabled via configuration");
        return Ok(());
    }

    let mut ubt = UbtExEx::new(&config)?;

    info!("UBT ExEx started with MDBX persistence");

    if let Some(head) = ubt.get_head() {
        info!(
            block = head.block.number,
            hash = %head.block.hash,
            "Resuming from persisted head, will backfill if needed"
        );
        ctx.notifications.set_with_head(head);
    } else {
        info!("No persisted head, starting fresh (will backfill from genesis)");
    }

    loop {
        tokio::select! {
            notification = ctx.notifications.try_next() => {
                match notification? {
                    Some(notification) => {
                        match &notification {
                            ExExNotification::ChainCommitted { new } => {
                                let tip = new.tip();
                                let tip_number = tip.number();
                                let tip_hash = tip.hash();
                                debug!(block = tip_number, hash = %tip_hash, "Processing committed chain");

                                ubt.process_chain(new.as_ref())?;
                                ubt.commit(tip_number, tip_hash)?;
                            }
                            ExExNotification::ChainReorged { old, new } => {
                                let old_tip = old.tip().number();
                                let new_tip = new.tip().number();
                                info!(from = old_tip, to = new_tip, "Handling reorg");

                                ubt.revert(old.as_ref())?;
                                ubt.process_chain(new.as_ref())?;
                                ubt.commit(new.tip().number(), new.tip().hash())?;
                            }
                            ExExNotification::ChainReverted { old } => {
                                let old_tip = old.tip().number();
                                info!(block = old_tip, "Handling revert");
                                ubt.revert(old.as_ref())?;
                            }
                        };

                        if let Some(committed_chain) = notification.committed_chain() {
                            ctx.events
                                .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
                        }
                    }
                    None => {
                        info!("Notification stream ended");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal (SIGINT)");
                ubt.shutdown()?;
                break;
            }
            _ = sigterm_recv() => {
                info!("Received shutdown signal (SIGTERM)");
                ubt.shutdown()?;
                break;
            }
        }
    }

    Ok(())
}

/// Platform-specific SIGTERM receiver.
/// On Unix, waits for SIGTERM. On other platforms, returns pending future.
#[cfg(unix)]
async fn sigterm_recv() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    sigterm.recv().await;
}

#[cfg(not(unix))]
async fn sigterm_recv() {
    std::future::pending::<()>().await;
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::UbtConfig;
    use tempfile::TempDir;

    /// Test harness for property-based testing of UbtExEx.
    ///
    /// Wraps a UbtExEx instance with a temporary MDBX database for isolated testing.
    /// Provides convenience methods for applying entries and taking snapshots.
    pub struct TestHarness {
        pub exex: UbtExEx,
        _temp_dir: TempDir,
    }

    impl TestHarness {
        /// Create a new test harness with a fresh temporary database.
        ///
        /// Uses flush_interval=1 and delta_retention=1024 for predictable behavior.
        pub fn new() -> Self {
            let temp_dir = TempDir::new().expect("failed to create temp dir");
            let config = UbtConfig::for_tests(temp_dir.path().to_path_buf());
            let exex = UbtExEx::new(&config).expect("failed to create UbtExEx");
            Self {
                exex,
                _temp_dir: temp_dir,
            }
        }

        /// Apply entries for a block and commit, returning the root hash.
        ///
        /// Adds the given entries to pending_entries and calls commit with the
        /// specified block number and hash.
        pub fn apply_entries_block(
            &mut self,
            block_number: u64,
            block_hash: B256,
            entries: Vec<(TreeKey, B256)>,
        ) -> B256 {
            self.exex.pending_entries.extend(entries);
            self.exex
                .commit(block_number, block_hash)
                .expect("commit failed")
        }

        /// Take a snapshot of all entries in the database.
        ///
        /// Returns entries in sorted order (by stem, then subindex).
        pub fn snapshot_entries(&self) -> Vec<(TreeKey, B256)> {
            self.exex
                .db
                .iter_entries_sorted()
                .expect("iter_entries_sorted failed")
        }

        /// Compute and return the current root hash via streaming.
        pub fn snapshot_root(&self) -> B256 {
            self.exex
                .compute_root_streaming()
                .expect("compute_root_streaming failed")
        }
    }

    #[test]
    fn test_harness_basic() {
        let mut harness = TestHarness::new();

        let stem = Stem::new([1u8; 31]);
        let key = TreeKey::new(stem, 0);
        let value = B256::repeat_byte(0x42);

        let root = harness.apply_entries_block(1, B256::repeat_byte(0x01), vec![(key, value)]);
        assert_ne!(root, B256::ZERO);

        let entries = harness.snapshot_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], (key, value));

        let snapshot_root = harness.snapshot_root();
        assert_eq!(root, snapshot_root);
    }

    #[test]
    fn test_apply_deltas_reverse() {
        let mut harness = TestHarness::new();

        let stem = Stem::new([2u8; 31]);
        let key = TreeKey::new(stem, 0);
        let initial_value = B256::repeat_byte(0x11);
        let updated_value = B256::repeat_byte(0x22);

        harness.apply_entries_block(1, B256::repeat_byte(0x01), vec![(key, initial_value)]);
        let root_after_block1 = harness.snapshot_root();

        harness.apply_entries_block(2, B256::repeat_byte(0x02), vec![(key, updated_value)]);

        let entries_before_revert = harness.snapshot_entries();
        assert_eq!(entries_before_revert[0].1, updated_value);

        let deltas = harness.exex.db.load_block_deltas(2).expect("load deltas");
        harness
            .exex
            .apply_deltas_reverse(2, &deltas)
            .expect("apply_deltas_reverse");

        let dirty: Vec<_> = harness.exex.dirty_stems.drain().collect();
        harness.exex.db.batch_update_stems(&dirty).unwrap();

        let entries_after_revert = harness.snapshot_entries();
        assert_eq!(entries_after_revert[0].1, initial_value);

        let root_after_revert = harness.snapshot_root();
        assert_eq!(root_after_block1, root_after_revert);
    }
}
