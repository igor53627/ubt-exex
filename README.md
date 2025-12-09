# reth-ubt-exex

Reth Execution Extension (ExEx) plugin for maintaining UBT (EIP-7864 Unified Binary Tree) state in parallel with the normal MPT state.

## Overview

This ExEx plugin receives block notifications from reth and maintains a Unified Binary Tree that tracks:
- Account basic data (nonce, balance, code size)
- Code hash and bytecode chunks
- Storage slots

The UBT root is computed and persisted after each block, enabling stateless Ethereum experimentation.

## Architecture

```
+-------------------------------------------------+
|                  reth node                       |
|  +-------------+     +----------------------+   |
|  | Consensus   |---->| ExExNotification     |   |
|  | Engine      |     | - ChainCommitted     |   |
|  +-------------+     | - ChainReorged       |   |
|                      | - ChainReverted      |   |
|                      +----------------------+   |
|                               |                 |
|                               v                 |
|                      +----------------------+   |
|                      |    UBT ExEx Plugin   |   |
|                      | - Extract state delta|   |
|                      | - Update UBT         |   |
|                      | - Compute root hash  |   |
|                      +----------------------+   |
|                               |                 |
|                               v                 |
|                      +----------------------+   |
|                      |    MDBX Database     |   |
|                      |    (persistent)      |   |
|                      +----------------------+   |
+-------------------------------------------------+
```

## Usage

### Build

```bash
cargo build --release
```

### Local Development

For development with local checkouts of `ubt` or `reth`, edit `.cargo/config.toml` and uncomment the relevant `[patch]` sections:

```bash
# Clone dependencies to sibling directories
git clone https://github.com/paradigmxyz/ubt ../ubt
git clone https://github.com/paradigmxyz/reth ../reth-ubt

# Edit .cargo/config.toml to uncomment patches, then build
cargo build --release
```

### Run with reth

```bash
# Run on Sepolia testnet
./target/release/reth-ubt node --chain sepolia

# Run on mainnet  
./target/release/reth-ubt node --chain mainnet

# With custom data directory
RETH_DATA_DIR=/path/to/data ./target/release/reth-ubt node --chain sepolia
```

### Output

The plugin persists UBT state to MDBX database at `$RETH_DATA_DIR/ubt/`:

```
ubt/
  data.mdb      # MDBX database with stem nodes
  lock.mdb      # MDBX lock file
```

The database contains:
- `ubt_stems` table: All stem nodes (31-byte stem -> serialized StemNode)
- `ubt_meta` table: Metadata including current head block and root hash

Logs show UBT updates per block:

```
INFO UBT ExEx started with MDBX persistence
INFO Loading UBT state from MDBX block=9507939 root=0x8c7f... stems=139347994
INFO Loaded stems from database loaded=139347994
INFO UBT updated and persisted block=9507940 entries=1234 stems=139348000 root=0x...
```

## How It Works

1. **Block Notification**: reth sends `ChainCommitted` notifications containing the execution outcome (state changes)

2. **State Extraction**: The plugin extracts from `BundleState`:
   - Account info changes (nonce, balance, code)
   - Storage slot changes
   - New bytecode deployments

3. **UBT Update**: For each change, compute the UBT key and insert:
   - `get_basic_data_key(address)` -> BasicDataLeaf
   - `get_code_hash_key(address)` -> code_hash
   - `get_code_chunk_key(address, i)` -> chunk[i]
   - `get_storage_slot_key(address, slot)` -> value

4. **Root Computation**: After processing all changes, compute `tree.root_hash()`

5. **Persistence**: Flush dirty stems to MDBX for restart recovery

## Configuration

Environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `RETH_DATA_DIR` | Base directory for data storage | `.` (current directory) |
| `UBT_FLUSH_INTERVAL` | Blocks between MDBX flushes | `1` |
| `UBT_DELTA_RETENTION` | Blocks to retain deltas for reorgs | `256` |

Example:

```bash
RETH_DATA_DIR=/data UBT_FLUSH_INTERVAL=10 UBT_DELTA_RETENTION=1000 \
  ./target/release/reth-ubt node --chain sepolia
```

## Backfill Support

The plugin supports automatic backfill when restarting:

1. On startup, it loads the persisted state from MDBX
2. If a head exists, it calls `ctx.notifications.set_with_head(head)`
3. reth's ExEx framework automatically backfills blocks from the persisted head to the current node head
4. This allows catching up after downtime without losing UBT state

**Fresh start (no persisted state):**
- Will process blocks as they arrive
- For existing synced nodes, use the migration tool first for faster initial sync

**Restart with persisted state:**
- Automatically backfills any blocks missed during downtime
- Continues from where it left off

## Metrics

Metrics are collected internally but not yet exposed. Planned metrics include:
- `ubt_block_processing_time_seconds` - Time to process each block
- `ubt_root_computation_time_seconds` - Time to compute root hash
- `ubt_persistence_time_seconds` - Time to flush to MDBX
- `ubt_stem_count` - Total number of stems in the tree
- `ubt_dirty_stems` - Number of stems pending flush

## Limitations

- **Initial Sync**: For an already-synced node, you may want to run the migration tool first to build the initial UBT state faster than backfilling from genesis
- **Memory**: Large state changes may require significant memory (full tree in RAM)

## Troubleshooting

### Root hash mismatch on startup

If you see "Root hash mismatch after loading", the database may be corrupted. Delete the `ubt/` directory and restart to rebuild from scratch.

### Out of memory

The full tree is kept in memory. For Sepolia (~100M stems), expect ~80GB+ RAM usage. Reduce memory by:
- Using a smaller chain (local devnet)
- Waiting for MDBX-backed reads optimization (#5)

### Slow block processing

If blocks take >100ms to process:
- Increase `UBT_FLUSH_INTERVAL` to batch writes
- Check disk I/O (MDBX is write-heavy)

## Integration with reth-ubt-migration

For existing nodes, first run the migration tool to build the initial UBT:

```bash
# 1. Run migration to build initial UBT state
cd ../reth-ubt-migration
cargo run --release -- --datadir ~/.local/share/reth/sepolia

# 2. Copy the UBT state (or start fresh with ExEx)
# 3. Run reth with UBT ExEx to maintain state going forward
cd ../reth-ubt-exex
./target/release/reth-ubt node --chain sepolia
```

## Documentation

- [Architecture](docs/architecture.md) - System design and data flow
- [Changelog](CHANGELOG.md) - Version history

## Roadmap

See [GitHub Issues](https://github.com/igor53627/reth-ubt-exex/issues) for optimization tracking:

| Priority | Issue | Description |
|----------|-------|-------------|
| P0 | #1 | Deferred root hash computation |
| P1 | #2, #3 | Batch persistence, reorg handling |
| P2 | #4, #5 | Incremental updates, MDBX-backed reads |
| P3 | #6, #7 | Streaming loader, parallel hashing |

## Dependencies

- [ubt](https://github.com/paradigmxyz/ubt) - Unified Binary Tree implementation
- [reth](https://github.com/paradigmxyz/reth) - Ethereum execution client

## License

MIT OR Apache-2.0
