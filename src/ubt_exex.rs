//! UBT ExEx implementation
//!
//! Maintains a Unified Binary Tree that tracks Ethereum state changes.
//! Persists tree data to MDBX for recovery after restarts.

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use futures::TryStreamExt;
use reth_ethereum::exex::{ExExContext, ExExEvent, ExExHead, ExExNotification};
use reth_exex::ExExNotificationsStream;
use reth_execution_types::Chain;
use reth_node_api::FullNodeComponents;
use reth_primitives_traits::{AlloyBlockHeader as _, NodePrimitives};
use std::{collections::HashMap, path::PathBuf};
use tracing::{debug, info, warn};
use ubt::{
    chunkify_code, get_basic_data_key, get_code_chunk_key, get_code_hash_key, get_storage_slot_key,
    BasicDataLeaf, Blake3Hasher, Stem, StemNode, StreamingTreeBuilder, TreeKey, UnifiedBinaryTree,
};

use crate::persistence::{UbtDatabase, UbtHead};

const UBT_DATA_DIR: &str = "ubt";

pub struct UbtExEx {
    tree: UnifiedBinaryTree<Blake3Hasher>,
    db: UbtDatabase,
    last_block: u64,
    last_hash: B256,
    pending_entries: Vec<(TreeKey, B256)>,
    dirty_stems: HashMap<Stem, StemNode>,
}

impl UbtExEx {
    pub fn new(data_dir: PathBuf) -> eyre::Result<Self> {
        let ubt_dir = data_dir.join(UBT_DATA_DIR);
        let db = UbtDatabase::open(&ubt_dir)?;

        let (tree, last_block, last_hash) = if let Some(head) = db.load_head()? {
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

            if verify_root_streaming(&db, head.root)? {
                info!("Streaming root verification passed");
            } else {
                warn!(
                    expected = %head.root,
                    "Streaming root verification failed - tree may be inconsistent"
                );
            }

            (tree, head.block_number, head.block_hash)
        } else {
            info!("Starting fresh UBT state");
            (UnifiedBinaryTree::new(), 0, B256::ZERO)
        };

        Ok(Self {
            tree,
            db,
            last_block,
            last_hash,
            pending_entries: Vec::new(),
            dirty_stems: HashMap::new(),
        })
    }

    pub fn get_head(&self) -> Option<ExExHead> {
        if self.last_block == 0 {
            None
        } else {
            Some(ExExHead::new(BlockNumHash::new(self.last_block, self.last_hash)))
        }
    }

    pub fn process_chain<N: NodePrimitives>(&mut self, chain: &Chain<N>) -> eyre::Result<()> {
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

    pub fn commit(&mut self, block_number: u64, block_hash: B256) -> eyre::Result<B256> {
        let entries = std::mem::take(&mut self.pending_entries);
        let entry_count = entries.len();

        for (key, value) in &entries {
            self.tree.insert(key.clone(), *value);

            let stem_node = self
                .dirty_stems
                .entry(key.stem)
                .or_insert_with(|| StemNode::new(key.stem));
            stem_node.set_value(key.subindex, *value);
        }

        let root = self.tree.root_hash();
        self.last_block = block_number;
        self.last_hash = block_hash;

        let dirty: Vec<_> = self.dirty_stems.drain().collect();
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

        info!(
            block = block_number,
            entries = entry_count,
            stems = self.tree.stem_count(),
            root = %root,
            "UBT updated and persisted"
        );

        Ok(root)
    }

    pub fn revert(&mut self, _chain: &Chain<impl NodePrimitives>) -> eyre::Result<()> {
        warn!("UBT revert requested - full rebuild required for accurate state");
        Ok(())
    }

    /// Get a stem node, checking dirty overlay first, then MDBX.
    /// Used for proof generation without requiring full tree in memory.
    pub fn get_stem(&self, stem: &Stem) -> eyre::Result<Option<StemNode>> {
        if let Some(node) = self.dirty_stems.get(stem) {
            return Ok(Some(node.clone()));
        }
        self.db.load_stem(stem)
    }

    /// Get a specific value by TreeKey, checking overlay then MDBX.
    pub fn get_value(&self, key: &TreeKey) -> eyre::Result<Option<B256>> {
        if let Some(node) = self.dirty_stems.get(&key.stem) {
            if let Some(value) = node.get_value(key.subindex) {
                return Ok(Some(value));
            }
        }
        Ok(self.tree.get(key))
    }
}

/// Verify root hash using streaming computation (memory-efficient).
fn verify_root_streaming(db: &UbtDatabase, expected_root: B256) -> eyre::Result<bool> {
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

pub async fn ubt_exex<Node: FullNodeComponents>(mut ctx: ExExContext<Node>) -> eyre::Result<()> {
    let data_dir = std::env::var("RETH_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    let mut ubt = UbtExEx::new(data_dir)?;

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

    while let Some(notification) = ctx.notifications.try_next().await? {
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

    Ok(())
}
