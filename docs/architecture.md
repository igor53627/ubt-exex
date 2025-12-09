# Architecture

## Overview

reth-ubt-exex is an Execution Extension (ExEx) that maintains EIP-7864 Unified Binary Tree state in parallel with reth's native MPT state.

```
┌───────────────────────────────────────────────────────────┐
│                         reth node                         │
│  ┌──────────────┐    ┌─────────────────────────────────┐  │
│  │   Consensus  │───>│      ExExNotification           │  │
│  │    Engine    │    │  - ChainCommitted (new blocks)  │  │
│  └──────────────┘    │  - ChainReorged (reorg)         │  │
│                      │  - ChainReverted (revert)       │  │
│                      └───────────────┬─────────────────┘  │
│                                      │                    │
│                                      v                    │
│                      ┌─────────────────────────────────┐  │
│                      │         UbtExEx                 │  │
│                      │  ┌───────────────────────────┐  │  │
│                      │  │   UnifiedBinaryTree       │  │  │
│                      │  │   (in-memory HashMap)     │  │  │
│                      │  └───────────────────────────┘  │  │
│                      │  ┌───────────────────────────┐  │  │
│                      │  │   dirty_stems overlay     │  │  │
│                      │  └───────────────────────────┘  │  │
│                      └───────────────┬─────────────────┘  │
│                                      │                    │
│                                      v                    │
│                      ┌─────────────────────────────────┐  │
│                      │         MDBX Database           │  │
│                      │  - ubt_stems: Stem -> StemNode  │  │
│                      │  - ubt_meta: head block/root    │  │
│                      └─────────────────────────────────┘  │
└───────────────────────────────────────────────────────────┘
```

## Data Flow

### Block Processing

1. **Notification Received**: reth sends `ChainCommitted { new: Chain }`
2. **State Extraction**: Extract changes from `BundleState`:
   - Account info (nonce, balance, code_size)
   - Storage slots
   - Deployed bytecode
3. **Key Derivation**: Convert to UBT keys:
   - `get_basic_data_key(address)` -> BasicDataLeaf
   - `get_code_hash_key(address)` -> code_hash
   - `get_code_chunk_key(address, i)` -> code chunk
   - `get_storage_slot_key(address, slot)` -> storage value
4. **Tree Update**: Insert entries into `UnifiedBinaryTree`
5. **Root Computation**: Call `tree.root_hash()` (currently rebuilds full tree)
6. **Persistence**: Write dirty stems to MDBX, save head metadata

### Startup/Recovery

1. **Load Head**: Read `ubt_meta` table for last block/root
2. **Load Stems**: Iterate `ubt_stems` table, insert into tree
3. **Verify Root**: Compare computed root vs stored root
4. **Set ExEx Head**: Call `ctx.notifications.set_with_head(head)`
5. **Backfill**: reth replays blocks from stored head to current tip

## Key Components

### UbtExEx (`ubt_exex.rs`)

Main ExEx struct holding:
- `tree: UnifiedBinaryTree<Blake3Hasher>` - in-memory tree
- `db: UbtDatabase` - MDBX handle
- `pending_entries: Vec<(TreeKey, B256)>` - current block's changes
- `dirty_stems: HashMap<Stem, StemNode>` - modified stems pending write

### UbtDatabase (`persistence.rs`)

MDBX wrapper with tables:
- `ubt_stems`: 31-byte stem key -> bincode-serialized `StemNode`
- `ubt_meta`: metadata key -> bincode-serialized `UbtHead`

### UnifiedBinaryTree (`ubt` crate)

Core tree implementation:
- `stems: HashMap<Stem, StemNode>` - all stems in memory
- `root: Node` - tree structure (rebuilt on mutations)
- `rebuild_root()` - O(n log n) full tree reconstruction

## Current Limitations

| Limitation                   | Impact                  | Status      |
|------------------------------|-------------------------|-------------|
| Full tree in memory          | 782GB RAM for Sepolia   | Open (#5, #6) |
| rebuild_root on every insert | O(N*S) per block        | Open (#4)   |
| ~~MDBX write every block~~   | ~~High I/O~~            | ✅ Fixed (#2) - configurable flush interval |
| ~~No reorg support~~         | ~~Corrupted state~~     | ✅ Fixed (#3) - delta-based reverts |

## Optimization Roadmap

```text
Phase 1 (P0-P1): Quick Wins
├── #1 Deferred root computation       [OPEN]
├── #2 Batch persistence               [DONE] - UBT_FLUSH_INTERVAL
└── #3 Reorg handling (correctness)    [DONE] - delta storage & reverts

Phase 2 (P2): Architectural Improvements  
├── #4 Incremental root updates        [OPEN]
└── #5 MDBX-backed reads               [OPEN]

Phase 3 (P3): Advanced Optimizations
├── #6 Streaming tree builder          [PARTIAL] - verify_root_streaming
└── #7 Parallel hashing                [OPEN]
```

## UBT Key Layout (EIP-7864)

```
TreeKey = Stem (31 bytes) + SubIndex (1 byte)

Account Basic Data:  hash(address || 0x00)[0:31] + 0x00
Code Hash:           hash(address || 0x00)[0:31] + 0x01  
Code Chunk i:        hash(address || 0x00)[0:31] + (0x80 + i)
Storage Slot:        hash(address || 0x01 || slot)[0:31] + slot[31]
```

## Dependencies

- **ubt crate** (github.com/paradigmxyz/ubt): Core tree, hashing, key derivation
- **reth** (github.com/paradigmxyz/reth): ExEx framework and MDBX bindings
- **reth-libmdbx**: Database backend
