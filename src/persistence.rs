//! MDBX persistence for UBT state.
//!
//! This module provides durable storage for UBT stem nodes and metadata using MDBX,
//! a fast key-value store optimized for read-heavy workloads.
//!
//! # Database Layout
//!
//! Three tables are used:
//! - `ubt_stems`: Maps 31-byte stem keys to serialized `StemNode` values
//! - `ubt_meta`: Stores metadata including the current head block and root hash
//! - `ubt_block_deltas`: Stores per-block state deltas for reorg handling
//!
//! # Recovery
//!
//! On startup, the ExEx loads all stems from MDBX and reconstructs the in-memory tree.
//! The stored root hash is verified against the computed root to detect corruption.

use alloy_primitives::{Address, B256};
use std::path::Path;
use tracing::warn;
use ubt::{Stem, StemNode, TreeKey, STEM_LEN};

use crate::error::{DatabaseError, Result, UbtError};
use crate::mdbx::{DatabaseFlags, Environment, Geometry, WriteFlags};

const STEMS_DB: &str = "ubt_stems";
const STEM_ADDR_DB: &str = "ubt_stem_addresses";
const META_DB: &str = "ubt_meta";
const DELTAS_DB: &str = "ubt_block_deltas";
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
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let max_size = mdbx_max_size_from_env().unwrap_or(1024 * 1024 * 1024 * 1024); // 1TB
        let geometry = Geometry {
            size_lower: 0,
            size_now: 4096 * 256,
            size_upper: max_size as i64,
            growth_step: -1,
            shrink_threshold: -1,
            page_size: 4096,
        };

        let env = Environment::builder()
            .set_max_dbs(10)
            .set_geometry(geometry)
            .open(path)
            .map_err(|e| {
                UbtError::Database(DatabaseError::Open {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })
            })?;

        let txn = env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        txn.create_db(Some(STEMS_DB), DatabaseFlags::CREATE)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.create_db(Some(META_DB), DatabaseFlags::CREATE)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.create_db(Some(DELTAS_DB), DatabaseFlags::CREATE)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.create_db(Some(STEM_ADDR_DB), DatabaseFlags::default())
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(Self { env })
    }

    pub fn load_head(&self) -> Result<Option<UbtHead>> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let meta_db = txn
            .open_db(Some(META_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        match txn
            .get::<Vec<u8>>(meta_db, META_KEY_HEAD)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
            Some(bytes) => {
                let head: UbtHead = bincode::deserialize(&bytes)?;
                Ok(Some(head))
            }
            None => Ok(None),
        }
    }

    pub fn save_head(&self, head: &UbtHead) -> Result<()> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let meta_db = txn
            .open_db(Some(META_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let bytes = bincode::serialize(head)?;
        txn.put(meta_db, META_KEY_HEAD, &bytes, WriteFlags::DEFAULT)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(())
    }

    pub fn load_stem(&self, stem: &Stem) -> Result<Option<StemNode>> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let stems_db = txn
            .open_db(Some(STEMS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        match txn
            .get::<Vec<u8>>(stems_db, stem.as_bytes())
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
            Some(bytes) => {
                let stem_node: StemNode = bincode::deserialize(&bytes)?;
                Ok(Some(stem_node))
            }
            None => Ok(None),
        }
    }

    /// Load a specific value by TreeKey from MDBX.
    pub fn load_value(&self, key: &TreeKey) -> Result<Option<B256>> {
        Ok(self
            .load_stem(&key.stem)?
            .and_then(|node| node.get_value(key.subindex)))
    }

    pub fn batch_update_stems(&self, updates: &[(Stem, StemNode)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let stems_db = txn
            .open_db(Some(STEMS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        for (stem, stem_node) in updates {
            let key = stem.as_bytes();
            let value = bincode::serialize(stem_node)?;
            txn.put(stems_db, key, &value, WriteFlags::DEFAULT)
                .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        }

        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn iter_stems(&self) -> Result<Vec<(Stem, StemNode)>> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let stems_db = txn
            .open_db(Some(STEMS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let mut stems = Vec::new();
        let mut cursor = txn
            .cursor(&stems_db)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        while let Some((key, value)) = cursor
            .next::<Vec<u8>, Vec<u8>>()
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
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
    pub fn sync(&self) -> Result<()> {
        self.env
            .sync(true)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        Ok(())
    }

    /// Iterate stems and yield (TreeKey, B256) pairs for streaming root computation.
    /// Entries are yielded in sorted order (by stem, then subindex).
    pub fn iter_entries_sorted(&self) -> Result<Vec<(TreeKey, B256)>> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let stems_db = txn
            .open_db(Some(STEMS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let mut entries = Vec::new();
        let mut cursor = txn
            .cursor(&stems_db)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        while let Some((key, value)) = cursor
            .next::<Vec<u8>, Vec<u8>>()
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
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

    pub fn save_stem_address(&self, stem: &Stem, address: &Address) -> Result<()> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let db = txn
            .open_db(Some(STEM_ADDR_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        if txn
            .get::<Vec<u8>>(db, stem.as_bytes())
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
            .is_some()
        {
            return Err(UbtError::Database(DatabaseError::Mdbx(format!(
                "Stem address already exists for {:?}",
                stem
            ))));
        }

        txn.put(
            db,
            stem.as_bytes(),
            address.as_slice(),
            WriteFlags::DEFAULT,
        )
        .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(())
    }

    pub fn batch_save_stem_addresses(&self, mappings: &[(Stem, Address)]) -> Result<()> {
        if mappings.is_empty() {
            return Ok(());
        }

        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let db = txn
            .open_db(Some(STEM_ADDR_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        for (stem, address) in mappings {
            if let Some(existing) = txn
                .get::<Vec<u8>>(db, stem.as_bytes())
                .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
            {
                if existing.as_slice() != address.as_slice() {
                    return Err(UbtError::Database(DatabaseError::Mdbx(
                        "Stem address mismatch in MDBX".to_string(),
                    )));
                }
                continue;
            }

            txn.put(
                db,
                stem.as_bytes(),
                address.as_slice(),
                WriteFlags::DEFAULT,
            )
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        }

        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(())
    }

    pub fn load_stem_address(&self, stem: &Stem) -> Result<Option<Address>> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let db = txn
            .open_db(Some(STEM_ADDR_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        match txn
            .get::<Vec<u8>>(db, stem.as_bytes())
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
            Some(bytes) => {
                if bytes.len() != 20 {
                    return Err(UbtError::Database(DatabaseError::Mdbx(format!(
                        "Invalid address length: expected 20, got {}",
                        bytes.len()
                    ))));
                }
                let mut addr = [0u8; 20];
                addr.copy_from_slice(&bytes);
                Ok(Some(Address::from(addr)))
            }
            None => Ok(None),
        }
    }

    pub fn save_block_deltas(&self, block_number: u64, deltas: &[(Stem, u8, B256)]) -> Result<()> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let deltas_db = txn
            .open_db(Some(DELTAS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let key = block_number.to_be_bytes();
        let value = bincode::serialize(deltas)?;
        txn.put(deltas_db, &key, &value, WriteFlags::DEFAULT)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(())
    }

    pub fn load_block_deltas(&self, block_number: u64) -> Result<Vec<(Stem, u8, B256)>> {
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let deltas_db = txn
            .open_db(Some(DELTAS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let key = block_number.to_be_bytes();
        match txn
            .get::<Vec<u8>>(deltas_db, &key)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
            Some(bytes) => {
                let deltas: Vec<(Stem, u8, B256)> = bincode::deserialize(&bytes)?;
                Ok(deltas)
            }
            None => Ok(Vec::new()),
        }
    }

    pub fn delete_block_deltas(&self, block_number: u64) -> Result<()> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let deltas_db = txn
            .open_db(Some(DELTAS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let key = block_number.to_be_bytes();
        txn.del(deltas_db, &key, None)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(())
    }

    /// Prune deltas for blocks older than the given block number.
    /// Returns the number of deltas deleted.
    pub fn prune_deltas_before(&self, block_number: u64) -> Result<usize> {
        let txn = self.env.begin_rw_txn().map_err(|e| {
            UbtError::Database(DatabaseError::Transaction(format!(
                "Failed to begin prune transaction: {}",
                e
            )))
        })?;
        let deltas_db = txn.open_db(Some(DELTAS_DB)).map_err(|e| {
            UbtError::Database(DatabaseError::Mdbx(format!(
                "Failed to open deltas db: {}",
                e
            )))
        })?;

        let mut cursor = txn.cursor(&deltas_db).map_err(|e| {
            UbtError::Database(DatabaseError::Mdbx(format!(
                "Failed to create cursor: {}",
                e
            )))
        })?;

        let mut to_delete = Vec::new();

        while let Some((key_bytes, _)) = cursor.next::<Vec<u8>, Vec<u8>>().map_err(|e| {
            UbtError::Database(DatabaseError::Mdbx(format!(
                "Cursor iteration failed: {}",
                e
            )))
        })? {
            if key_bytes.len() == 8 {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&key_bytes);
                let bn = u64::from_be_bytes(arr);
                if bn < block_number {
                    to_delete.push(key_bytes);
                }
            }
        }
        drop(cursor);

        let count = to_delete.len();
        for key in to_delete {
            txn.del(deltas_db, &key, None).map_err(|e| {
                UbtError::Database(DatabaseError::Mdbx(format!(
                    "Failed to delete delta: {}",
                    e
                )))
            })?;
        }
        txn.commit().map_err(|e| {
            UbtError::Database(DatabaseError::Transaction(format!(
                "Failed to commit prune: {}",
                e
            )))
        })?;

        Ok(count)
    }

    #[allow(dead_code)]
    pub fn delete_deltas_after(&self, block_number: u64) -> Result<()> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;
        let deltas_db = txn
            .open_db(Some(DELTAS_DB))
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;

        let mut cursor = txn
            .cursor(&deltas_db)
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        let mut to_delete = Vec::new();

        while let Some((key_bytes, _)) = cursor
            .next::<Vec<u8>, Vec<u8>>()
            .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?
        {
            if key_bytes.len() == 8 {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&key_bytes);
                let bn = u64::from_be_bytes(arr);
                if bn > block_number {
                    to_delete.push(key_bytes);
                }
            }
        }
        drop(cursor);

        for key in to_delete {
            txn.del(deltas_db, &key, None)
                .map_err(|e| UbtError::Database(DatabaseError::Mdbx(e.to_string())))?;
        }
        txn.commit()
            .map_err(|e| UbtError::Database(DatabaseError::Transaction(e.to_string())))?;

        Ok(())
    }
}

fn mdbx_max_size_from_env() -> Option<usize> {
    let raw = std::env::var("UBT_MDBX_MAX_SIZE").ok()?;
    let s = raw.trim().to_ascii_uppercase();
    let split = s
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split);
    let number: u64 = match num_str.trim().parse() {
        Ok(v) => v,
        Err(_) => {
            warn!(value = %raw, "Invalid UBT_MDBX_MAX_SIZE, expected integer with optional unit");
            return None;
        }
    };

    let factor: u64 = match unit.trim() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        "T" | "TB" => 1024_u64 * 1024 * 1024 * 1024,
        other => {
            warn!(value = %raw, unit = %other, "Invalid UBT_MDBX_MAX_SIZE unit");
            return None;
        }
    };

    number.checked_mul(factor).map(|v| v as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_db() -> (TempDir, UbtDatabase) {
        let dir = TempDir::new().unwrap();
        let db = UbtDatabase::open(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_open_creates_tables() {
        let (_dir, _db) = create_test_db();
    }

    #[test]
    fn test_save_load_head() {
        let (_dir, db) = create_test_db();

        let head = UbtHead {
            block_number: 100,
            block_hash: B256::repeat_byte(0x42),
            root: B256::repeat_byte(0xAB),
            stem_count: 1000,
        };

        db.save_head(&head).unwrap();
        let loaded = db.load_head().unwrap().unwrap();

        assert_eq!(loaded.block_number, head.block_number);
        assert_eq!(loaded.block_hash, head.block_hash);
        assert_eq!(loaded.root, head.root);
        assert_eq!(loaded.stem_count, head.stem_count);
    }

    #[test]
    fn test_load_head_empty() {
        let (_dir, db) = create_test_db();
        assert!(db.load_head().unwrap().is_none());
    }

    #[test]
    fn test_batch_update_and_iter_stems() {
        let (_dir, db) = create_test_db();

        let stem1 = Stem::new([1u8; STEM_LEN]);
        let stem2 = Stem::new([2u8; STEM_LEN]);

        let mut node1 = StemNode::new(stem1);
        node1.set_value(0, B256::repeat_byte(0x11));
        node1.set_value(1, B256::repeat_byte(0x12));

        let mut node2 = StemNode::new(stem2);
        node2.set_value(0, B256::repeat_byte(0x21));

        db.batch_update_stems(&[(stem1, node1.clone()), (stem2, node2.clone())])
            .unwrap();

        let stems = db.iter_stems().unwrap();
        assert_eq!(stems.len(), 2);
    }

    #[test]
    fn test_load_stem() {
        let (_dir, db) = create_test_db();

        let stem = Stem::new([3u8; STEM_LEN]);
        let mut node = StemNode::new(stem);
        node.set_value(5, B256::repeat_byte(0x55));

        db.batch_update_stems(&[(stem, node.clone())]).unwrap();

        let loaded = db.load_stem(&stem).unwrap().unwrap();
        assert_eq!(loaded.get_value(5), Some(B256::repeat_byte(0x55)));
    }

    #[test]
    fn test_block_deltas_roundtrip() {
        let (_dir, db) = create_test_db();

        let stem = Stem::new([4u8; STEM_LEN]);
        let deltas = vec![
            (stem, 0u8, B256::repeat_byte(0xAA)),
            (stem, 1u8, B256::repeat_byte(0xBB)),
        ];

        db.save_block_deltas(100, &deltas).unwrap();
        let loaded = db.load_block_deltas(100).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].2, B256::repeat_byte(0xAA));
    }

    #[test]
    fn test_delete_block_deltas() {
        let (_dir, db) = create_test_db();

        let stem = Stem::new([5u8; STEM_LEN]);
        db.save_block_deltas(100, &[(stem, 0, B256::ZERO)]).unwrap();

        db.delete_block_deltas(100).unwrap();

        let loaded = db.load_block_deltas(100).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_prune_deltas_before() {
        let (_dir, db) = create_test_db();

        let stem = Stem::new([6u8; STEM_LEN]);
        db.save_block_deltas(50, &[(stem, 0, B256::ZERO)]).unwrap();
        db.save_block_deltas(100, &[(stem, 1, B256::ZERO)]).unwrap();
        db.save_block_deltas(150, &[(stem, 2, B256::ZERO)]).unwrap();

        let pruned = db.prune_deltas_before(100).unwrap();
        assert_eq!(pruned, 1);

        assert!(db.load_block_deltas(50).unwrap().is_empty());
        assert!(!db.load_block_deltas(100).unwrap().is_empty());
        assert!(!db.load_block_deltas(150).unwrap().is_empty());
    }

    #[test]
    fn test_delete_block_deltas_idempotent() {
        let (_dir, db) = create_test_db();

        // Deleting a non-existent block should succeed (idempotent operation).
        // This tests that MDBX_NOTFOUND is handled correctly.
        db.delete_block_deltas(999999).unwrap();

        // Verify the operation is truly idempotent by calling it again.
        db.delete_block_deltas(999999).unwrap();
    }

    #[test]
    fn test_stem_address_roundtrip() {
        let (_dir, db) = create_test_db();

        let stem = Stem::new([7u8; STEM_LEN]);
        let address = Address::repeat_byte(0x42);

        db.save_stem_address(&stem, &address).unwrap();
        let loaded = db.load_stem_address(&stem).unwrap().unwrap();

        assert_eq!(loaded, address);
    }

    #[test]
    fn test_stem_address_not_found() {
        let (_dir, db) = create_test_db();

        let stem = Stem::new([8u8; STEM_LEN]);
        let loaded = db.load_stem_address(&stem).unwrap();

        assert!(loaded.is_none());
    }

    #[test]
    fn test_batch_save_stem_addresses() {
        let (_dir, db) = create_test_db();

        let stem1 = Stem::new([9u8; STEM_LEN]);
        let stem2 = Stem::new([10u8; STEM_LEN]);
        let address1 = Address::repeat_byte(0x11);
        let address2 = Address::repeat_byte(0x22);

        db.batch_save_stem_addresses(&[(stem1, address1), (stem2, address2)])
            .unwrap();

        assert_eq!(db.load_stem_address(&stem1).unwrap().unwrap(), address1);
        assert_eq!(db.load_stem_address(&stem2).unwrap().unwrap(), address2);
    }
}
