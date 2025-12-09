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
//! 3. Updates the in-memory tree and dirty overlay
//! 4. Periodically flushes to MDBX for durability
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
    BasicDataLeaf, Blake3Hasher, Stem, StemNode, StreamingTreeBuilder, TreeKey, UnifiedBinaryTree,
};

use crate::config::UbtConfig;
use crate::error::Result;
use crate::persistence::{UbtDatabase, UbtHead};

const UBT_DATA_DIR: &str = "ubt";

pub struct UbtExEx {
    tree: UnifiedBinaryTree<Blake3Hasher>,
    db: UbtDatabase,
    last_block: u64,
    last_hash: B256,
    pending_entries: Vec<(TreeKey, B256)>,
    dirty_stems: HashMap<Stem, StemNode>,
    flush_interval: u64,
    delta_retention: u64,
    last_persisted_block: u64,
    last_persisted_hash: B256,
}

impl UbtExEx {
    /// Create a new UBT ExEx instance with the given configuration.
    pub fn new(config: &UbtConfig) -> Result<Self> {
        let data_dir = config.get_data_dir();
        let ubt_dir = data_dir.join(UBT_DATA_DIR);
        let db = UbtDatabase::open(&ubt_dir)?;
        let flush_interval = config.get_flush_interval();
        let delta_retention = config.get_delta_retention();

        let (tree, last_block, last_hash, last_persisted_block, last_persisted_hash) = if let Some(head) =
            db.load_head()?
        {
            info!(
                block = head.block_number,
                root = %head.root,
                stems = head.stem_count,
                "Loading UBT state from MDBX"
            );

            let mut tree = UnifiedBinaryTree::with_capacity(head.stem_count);

            let stems = db.iter_stems()?;
            info!(loaded = stems.len(), "Loaded stems from database");

            for (stem, stem_node) in stems {
                for (&subindex, &value) in &stem_node.values {
                    let key = TreeKey::new(stem, subindex);
                    tree.insert(key, value);
                }
            }

            let root = tree.root_hash();
            if root != head.root {
                warn!(
                    expected = %head.root,
                    actual = %root,
                    "Root hash mismatch after loading - tree may be inconsistent"
                );
            }

            info!("Verifying UBT root via streaming; this may take a while on large state");
            if verify_root_streaming(&db, head.root)? {
                info!("Streaming root verification passed");
            } else {
                warn!(
                    expected = %head.root,
                    "Streaming root verification failed - tree may be inconsistent"
                );
            }

            (tree, head.block_number, head.block_hash, head.block_number, head.block_hash)
        } else {
            info!("Starting fresh UBT state");
            (UnifiedBinaryTree::new(), 0, B256::ZERO, 0, B256::ZERO)
        };

        info!(
            flush_interval = flush_interval,
            delta_retention = delta_retention,
            "UBT flush interval configured"
        );

        Ok(Self {
            tree,
            db,
            last_block,
            last_hash,
            pending_entries: Vec::new(),
            dirty_stems: HashMap::new(),
            flush_interval,
            delta_retention,
            last_persisted_block,
            last_persisted_hash,
        })
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

    pub fn commit(&mut self, block_number: u64, block_hash: B256) -> Result<B256> {
        let entries = std::mem::take(&mut self.pending_entries);
        let entry_count = entries.len();

        let mut deltas: Vec<(Stem, u8, B256)> = Vec::new();

        for (key, value) in &entries {
            let old_value = self
                .dirty_stems
                .get(&key.stem)
                .and_then(|node| node.get_value(key.subindex))
                .or_else(|| self.tree.get(key))
                .unwrap_or(B256::ZERO);

            if old_value != *value {
                deltas.push((key.stem, key.subindex, old_value));
            }

            self.tree.insert(key.clone(), *value);

            let stem_node = self
                .dirty_stems
                .entry(key.stem)
                .or_insert_with(|| StemNode::new(key.stem));
            stem_node.set_value(key.subindex, *value);
        }

        if !deltas.is_empty() {
            self.db.save_block_deltas(block_number, &deltas)?;
        }

        let root_start = Instant::now();
        let root = self.tree.root_hash();
        crate::metrics::record_root_computation(root_start.elapsed().as_secs_f64());

        self.last_block = block_number;
        self.last_hash = block_hash;

        let should_flush = block_number <= self.last_persisted_block ||
            (block_number - self.last_persisted_block) >= self.flush_interval;

        if should_flush {
            let persist_start = Instant::now();
            let dirty: Vec<_> = self.dirty_stems.drain().collect();
            let dirty_count = dirty.len();
            if !dirty.is_empty() {
                self.db.batch_update_stems(&dirty)?;
            }

            let head = UbtHead {
                block_number,
                block_hash,
                root,
                stem_count: self.tree.stem_count(),
            };
            self.db.save_head(&head)?;
            crate::metrics::record_persistence(persist_start.elapsed().as_secs_f64(), dirty_count);
            crate::metrics::record_dirty_stems(0);

            self.last_persisted_block = block_number;
            self.last_persisted_hash = block_hash;

            info!(
                block = block_number,
                entries = entry_count,
                stems = self.tree.stem_count(),
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
        } else {
            crate::metrics::record_dirty_stems(self.dirty_stems.len());
            debug!(
                block = block_number,
                entries = entry_count,
                pending_stems = self.dirty_stems.len(),
                blocks_until_flush = self.flush_interval - (block_number - self.last_persisted_block),
                root = %root,
                "UBT updated in-memory (pending flush)"
            );
        }

        crate::metrics::record_block_processed(block_number, entry_count, self.tree.stem_count());

        Ok(root)
    }

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

            for (stem, subindex, old_value) in deltas.iter().rev() {
                let key = TreeKey::new(*stem, *subindex);
                self.tree.insert(key.clone(), *old_value);

                let stem_node = self
                    .dirty_stems
                    .entry(*stem)
                    .or_insert_with(|| StemNode::new(*stem));
                stem_node.set_value(*subindex, *old_value);
            }

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

            let root = self.tree.root_hash();
            let head = UbtHead {
                block_number: self.last_block,
                block_hash: self.last_hash,
                root,
                stem_count: self.tree.stem_count(),
            };
            self.db.save_head(&head)?;

            self.last_persisted_block = self.last_block;
            self.last_persisted_hash = self.last_hash;
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
        Ok(self.tree.get(key))
    }

    /// Gracefully shutdown, flushing all pending state to MDBX.
    pub fn shutdown(&mut self) -> Result<()> {
        info!("UBT ExEx shutting down, flushing pending state...");

        let dirty: Vec<_> = self.dirty_stems.drain().collect();
        if !dirty.is_empty() {
            info!(stems = dirty.len(), "Flushing dirty stems");
            self.db.batch_update_stems(&dirty)?;
        }

        let root = self.tree.root_hash();
        let head = UbtHead {
            block_number: self.last_block,
            block_hash: self.last_hash,
            root,
            stem_count: self.tree.stem_count(),
        };
        self.db.save_head(&head)?;

        info!(
            block = self.last_block,
            root = %root,
            "UBT ExEx shutdown complete"
        );

        Ok(())
    }
}

/// Verify root hash using streaming computation (memory-efficient).
fn verify_root_streaming(db: &UbtDatabase, expected_root: B256) -> Result<bool> {
    let entries = db.iter_entries_sorted()?;
    let computed = StreamingTreeBuilder::<Blake3Hasher>::new().build_root_hash(entries);
    Ok(computed == expected_root)
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
