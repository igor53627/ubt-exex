//! MDBX persistence for UBT state.
//!
//! Stores UBT stems and their values in MDBX tables for efficient
//! persistence and recovery.

use alloy_primitives::B256;
use reth_libmdbx::{DatabaseFlags, Environment, Geometry, PageSize, WriteFlags};
use std::path::Path;
use ubt::{Stem, StemNode, TreeKey, STEM_LEN};

const STEMS_DB: &str = "ubt_stems";
const META_DB: &str = "ubt_meta";
const META_KEY_HEAD: &[u8] = b"head";

pub struct UbtDatabase {
    env: Environment,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UbtHead {
    pub block_number: u64,
    pub block_hash: B256,
    pub root: B256,
    pub stem_count: usize,
}

impl UbtDatabase {
    pub fn open(path: &Path) -> eyre::Result<Self> {
        std::fs::create_dir_all(path)?;

        let mut builder = Environment::builder();
        builder.set_max_dbs(10);
        builder.set_geometry(Geometry {
            size: Some(0..(1024 * 1024 * 1024 * 1024)), // Up to 1TB
            page_size: Some(PageSize::Set(4096)),
            ..Default::default()
        });

        let env = builder.open(path)?;

        let txn = env.begin_rw_txn()?;
        txn.create_db(Some(STEMS_DB), DatabaseFlags::default())?;
        txn.create_db(Some(META_DB), DatabaseFlags::default())?;
        txn.commit()?;

        Ok(Self { env })
    }

    pub fn load_head(&self) -> eyre::Result<Option<UbtHead>> {
        let txn = self.env.begin_ro_txn()?;
        let meta_db = txn.open_db(Some(META_DB))?;

        match txn.get::<Vec<u8>>(meta_db.dbi(), META_KEY_HEAD)? {
            Some(bytes) => {
                let head: UbtHead = bincode::deserialize(&bytes)?;
                Ok(Some(head))
            }
            None => Ok(None),
        }
    }

    pub fn save_head(&self, head: &UbtHead) -> eyre::Result<()> {
        let txn = self.env.begin_rw_txn()?;
        let meta_db = txn.open_db(Some(META_DB))?;

        let bytes = bincode::serialize(head)?;
        txn.put(meta_db.dbi(), META_KEY_HEAD, &bytes, WriteFlags::default())?;
        txn.commit()?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn load_stem(&self, stem: &Stem) -> eyre::Result<Option<StemNode>> {
        let txn = self.env.begin_ro_txn()?;
        let stems_db = txn.open_db(Some(STEMS_DB))?;

        match txn.get::<Vec<u8>>(stems_db.dbi(), stem.as_bytes())? {
            Some(bytes) => {
                let stem_node: StemNode = bincode::deserialize(&bytes)?;
                Ok(Some(stem_node))
            }
            None => Ok(None),
        }
    }

    pub fn batch_update_stems(&self, updates: &[(Stem, StemNode)]) -> eyre::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let txn = self.env.begin_rw_txn()?;
        let stems_db = txn.open_db(Some(STEMS_DB))?;

        for (stem, stem_node) in updates {
            let key = stem.as_bytes();
            let value = bincode::serialize(stem_node)?;
            txn.put(stems_db.dbi(), key, &value, WriteFlags::default())?;
        }

        txn.commit()?;
        Ok(())
    }

    pub fn iter_stems(&self) -> eyre::Result<Vec<(Stem, StemNode)>> {
        let txn = self.env.begin_ro_txn()?;
        let stems_db = txn.open_db(Some(STEMS_DB))?;

        let mut stems = Vec::new();
        let mut cursor = txn.cursor(&stems_db)?;

        while let Some((key, value)) = cursor.next::<Vec<u8>, Vec<u8>>()? {
            if key.len() == STEM_LEN {
                let mut stem_bytes = [0u8; STEM_LEN];
                stem_bytes.copy_from_slice(&key);
                let stem = Stem::new(stem_bytes);
                let stem_node: StemNode = bincode::deserialize(&value)?;
                stems.push((stem, stem_node));
            }
        }

        Ok(stems)
    }

    #[allow(dead_code)]
    pub fn sync(&self) -> eyre::Result<()> {
        self.env.sync(true)?;
        Ok(())
    }

    /// Iterate stems and yield (TreeKey, B256) pairs for streaming root computation.
    /// Entries are yielded in sorted order (by stem, then subindex).
    pub fn iter_entries_sorted(&self) -> eyre::Result<Vec<(TreeKey, B256)>> {
        let txn = self.env.begin_ro_txn()?;
        let stems_db = txn.open_db(Some(STEMS_DB))?;

        let mut entries = Vec::new();
        let mut cursor = txn.cursor(&stems_db)?;

        while let Some((key, value)) = cursor.next::<Vec<u8>, Vec<u8>>()? {
            if key.len() == STEM_LEN {
                let mut stem_bytes = [0u8; STEM_LEN];
                stem_bytes.copy_from_slice(&key);
                let stem = Stem::new(stem_bytes);
                let stem_node: StemNode = bincode::deserialize(&value)?;

                let mut subindices: Vec<_> = stem_node.values.keys().copied().collect();
                subindices.sort();

                for subindex in subindices {
                    if let Some(&value) = stem_node.values.get(&subindex) {
                        let tree_key = TreeKey::new(stem, subindex);
                        entries.push((tree_key, value));
                    }
                }
            }
        }

        Ok(entries)
    }
}
