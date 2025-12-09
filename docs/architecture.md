# Architecture

## Overview

reth-ubt-exex is an Execution Extension (ExEx) that maintains EIP-7864 Unified Binary Tree state in parallel with reth's native MPT state.

```text
+-----------------------------------------------------------+
|                         reth node                         |
|  +--------------+    +---------------------------------+  |
|  |   Consensus  |--->|      ExExNotification           |  |
|  |    Engine    |    |  - ChainCommitted (new blocks)  |  |
|  +--------------+    |  - ChainReorged (reorg)         |  |
|                      |  - ChainReverted (revert)       |  |
|                      +---------------+-----------------+  |
|                                      |                    |
|                                      v                    |
|                      +---------------------------------+  |
|                      |         UbtExEx                 |  |
|                      |  +---------------------------+  |  |
|                      |  |   UnifiedBinaryTree       |  |  |
|                      |  |   (in-memory HashMap)     |  |  |
|                      |  +---------------------------+  |  |
|                      |  +---------------------------+  |  |
|                      |  |   dirty_stems overlay     |  |  |
|                      |  +---------------------------+  |  |
|                      +---------------+-----------------+  |
|                                      |                    |
|                                      v                    |
|                      +---------------------------------+  |
|                      |         MDBX Database           |  |
|                      |  - ubt_stems: Stem -> StemNode  |  |
|                      |  - ubt_meta: head block/root    |  |
|                      |  - ubt_block_deltas: reorg data |  |
|                      +---------------------------------+  |
+-----------------------------------------------------------+
```text

## Production Readiness

| Aspect | Status | Notes |
|--------|--------|-------|
| Correctness | Ready | Reorg handling with delta-based reverts |
| Error handling | Ready | Custom error types (UbtError, DatabaseError) |
| Observability | Ready | Prometheus-compatible metrics |
| Graceful shutdown | Ready | SIGINT/SIGTERM handlers flush state |
| Configuration | Ready | CLI args + env var fallbacks |
| Persistence | Ready | MDBX with configurable flush interval |
| Delta pruning | Ready | Configurable retention (default 256 blocks) |
| Memory | Ready | MDBX-backed reads, <1GB baseline (spike during root computation) |
| Performance | Acceptable | O(S log S) per flush, streaming root computation |

**Status**: Ready for production deployment on testnet and mainnet.

## Configuration

Environment variables (preferred until CLI wiring complete):

| Variable | Description | Default |
|----------|-------------|---------|
| `RETH_DATA_DIR` | Base directory for data storage | `.` (current directory) |
| `UBT_FLUSH_INTERVAL` | Blocks between MDBX flushes | `1` |
| `UBT_DELTA_RETENTION` | Blocks to retain deltas for reorgs | `256` |

CLI arguments are defined in `UbtConfig` but not yet wired through reth's extension system.

## Data Flow

### Block Processing

1. **Notification Received**: reth sends `ChainCommitted { new: Chain }`
2. **State Extraction**: Extract changes from `BundleState`:
   - Account info (nonce, balance, code_size)
   - Storage slots
   - Deployed bytecode
3. **Delta Recording**: Store old values for reorg support
4. **Key Derivation**: Convert to UBT keys:
   - `get_basic_data_key(address)` -> BasicDataLeaf
   - `get_code_hash_key(address)` -> code_hash
   - `get_code_chunk_key(address, i)` -> code chunk
   - `get_storage_slot_key(address, slot)` -> storage value
5. **Tree Update**: Insert entries into `UnifiedBinaryTree`
6. **Root Computation**: Call `tree.root_hash()` (O(S log S) where S = stems)
7. **Persistence**: On flush interval, write dirty stems to MDBX
8. **Delta Pruning**: Remove deltas older than `delta_retention` blocks

### Reorg Handling

1. **Revert Notification**: reth sends `ChainReorged { old, new }` or `ChainReverted { old }`
2. **Load Deltas**: Read stored deltas for reverted blocks
3. **Apply Reverts**: Restore old values in LIFO order (newest block first)
4. **Update Head**: Set new head to parent of first reverted block
5. **Persist if Needed**: Flush if reorg affected persisted blocks

### Startup/Recovery

1. **Load Head**: Read `ubt_meta` table for last block/root
2. **Load Stems**: Iterate `ubt_stems` table, insert into tree
3. **Verify Root**: Compare computed root vs stored root (streaming verification)
4. **Set ExEx Head**: Call `ctx.notifications.set_with_head(head)`
5. **Backfill**: reth replays blocks from stored head to current tip

### Graceful Shutdown

1. **Signal Received**: SIGINT (Ctrl+C) or SIGTERM
2. **Flush Dirty Stems**: Write all pending stems to MDBX
3. **Save Head**: Persist current block number, hash, and root
4. **Exit**: Process terminates cleanly

## Key Components

### UbtExEx (`ubt_exex.rs`)

Main ExEx struct:

| Field | Type | Purpose |
|-------|------|---------|
| `tree` | `UnifiedBinaryTree<Blake3Hasher>` | In-memory tree |
| `db` | `UbtDatabase` | MDBX handle |
| `pending_entries` | `Vec<(TreeKey, B256)>` | Current block changes |
| `dirty_stems` | `HashMap<Stem, StemNode>` | Modified stems pending write |
| `flush_interval` | `u64` | Blocks between MDBX writes |
| `delta_retention` | `u64` | Blocks to retain deltas |
| `last_persisted_block` | `u64` | Last flushed block number |

### UbtDatabase (`persistence.rs`)

MDBX wrapper with three tables:

| Table | Key | Value | Purpose |
|-------|-----|-------|---------|
| `ubt_stems` | 31-byte stem | bincode `StemNode` | Tree data |
| `ubt_meta` | `"head"` | bincode `UbtHead` | Checkpoint |
| `ubt_block_deltas` | block number (u64 BE) | bincode deltas | Reorg support |

### Error Types (`error.rs`)

```text
UbtError
  +-- Database(DatabaseError)
  |     +-- Mdbx(String)
  |     +-- Open { path, reason }
  |     +-- Transaction(String)
  +-- Serialization(bincode::Error)
  +-- StateExtraction { message }
  +-- Io(std::io::Error)
```text

### Metrics (`metrics.rs`)

| Metric | Type | Description |
|--------|------|-------------|
| `ubt_exex_blocks_processed_total` | Counter | Total blocks processed |
| `ubt_exex_entries_per_block` | Histogram | State changes per block |
| `ubt_exex_stems_total` | Gauge | Total stems in tree |
| `ubt_exex_last_block_number` | Gauge | Current block number |
| `ubt_exex_root_computation_seconds` | Histogram | Root hash time |
| `ubt_exex_persistence_seconds` | Histogram | MDBX write time |
| `ubt_exex_stems_persisted` | Histogram | Stems written per flush |
| `ubt_exex_dirty_stems` | Gauge | Pending stems |
| `ubt_exex_reverts_total` | Counter | Revert operations |
| `ubt_exex_revert_blocks` | Histogram | Blocks per revert |
| `ubt_exex_revert_entries` | Histogram | Entries per revert |

### UnifiedBinaryTree (`ubt` crate)

Core tree implementation:
- `stems: HashMap<Stem, StemNode>` - all stems in memory
- `root_hash()` - O(S log S) tree reconstruction
- `StreamingTreeBuilder` - memory-efficient root verification

## Current Limitations

| Limitation | Impact | Status |
|------------|--------|--------|
| O(S log S) root computation | Memory spike during flush | Open (#4) |
| CLI args not wired | Must use env vars | Open |
| Root only on flush | Returns cached root between flushes | By design |

## Optimization Roadmap

```text
Phase 1 (P0-P1): Quick Wins
|-- #1 Deferred root computation       [DONE] - once per root_hash() call
|-- #2 Batch persistence               [DONE] - UBT_FLUSH_INTERVAL
+-- #3 Reorg handling                  [DONE] - delta storage and reverts

Phase 2 (P2): Memory Optimization
|-- #23 Remove full tree at startup    [DONE] - MDBX is canonical
|-- #24 MDBX-backed reads              [DONE] - dirty overlay + MDBX fallback
+-- #25 Streaming root computation     [DONE] - StreamingTreeBuilder from MDBX

Phase 3 (P3): Advanced Optimizations
|-- #4 Incremental root updates        [OPEN] - requires ubt crate changes
+-- #7 Parallel hashing                [DONE] - rayon for stem hashing
```text

## UBT Key Layout (EIP-7864)

```text
TreeKey = Stem (31 bytes) + SubIndex (1 byte)

Account Basic Data:  hash(address || 0x00)[0:31] + 0x00
Code Hash:           hash(address || 0x00)[0:31] + 0x01
Code Chunk i:        hash(address || 0x00)[0:31] + (0x80 + i)
Storage Slot:        hash(address || 0x01 || slot)[0:31] + slot[31]
```text

## Dependencies

| Crate | Source | Purpose |
|-------|--------|---------|
| `ubt` | github.com/igor53627/ubt-rs | Core tree, hashing, key derivation |
| `reth-*` | github.com/paradigmxyz/reth | ExEx framework, MDBX bindings |
| `thiserror` | crates.io | Error type derives |
| `metrics` | crates.io | Prometheus-compatible metrics |

For local development, uncomment patches in `.cargo/config.toml` to use sibling directories.
