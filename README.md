# reth-ubt-exex

Reth Execution Extension (ExEx) plugin for maintaining UBT (EIP-7864 Unified Binary Tree) state in parallel with the normal MPT state.

## Overview

This ExEx plugin receives block notifications from reth and maintains a Unified Binary Tree that tracks:
- Account basic data (nonce, balance, code size)
- Code hash and bytecode chunks
- Storage slots

The UBT root is computed via streaming from MDBX and persisted on each flush, enabling stateless Ethereum experimentation.

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
|                      | - dirty_stems overlay|   |
|                      | - MDBX state store   |   |
|                      | - Streaming root     |   |
|                      +----------------------+   |
|                               |                 |
|                               v                 |
|                      +----------------------+   |
|                      |    MDBX Database     |   |
|                      |  (canonical state)   |   |
|                      +----------------------+   |
+-------------------------------------------------+
```

## Memory Model

State is stored in MDBX (canonical) with a small dirty overlay in memory:
- Full tree is NOT loaded at startup
- Reads: dirty overlay -> MDBX fallback
- Writes: accumulate in overlay, flush to MDBX periodically
- Root: computed via streaming from MDBX on flush

**Memory usage**: <1GB baseline (spike during root computation)

## Usage

### Build

```bash
cargo build --release
```

### Local Development

For development with local checkouts of `ubt` or `reth`, edit `.cargo/config.toml` and uncomment the relevant `[patch]` sections:

```bash
# Clone dependencies to sibling directories
git clone https://github.com/igor53627/ubt-rs ../ubt-rs
git clone https://github.com/paradigmxyz/reth ../reth

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
- `ubt_block_deltas` table: Per-block deltas for reorg handling

Logs show UBT updates:

```text
INFO Resuming UBT state from MDBX (not loading full tree) block=9507939 root=0x8c7f... stems=139347994
INFO Verifying UBT root via streaming
INFO Streaming root verification passed
INFO UBT updated and flushed to MDBX block=9507940 entries=1234 stems=139348000 root=0x...
```

## How It Works

1. **Block Notification**: reth sends `ChainCommitted` notifications containing the execution outcome

2. **State Extraction**: Extract from `BundleState`:
   - Account info changes (nonce, balance, code)
   - Storage slot changes
   - New bytecode deployments

3. **Delta Recording**: Store old values for reorg support

4. **Overlay Update**: Update dirty stems overlay (in-memory)

5. **Flush to MDBX**: On flush interval, write overlay to MDBX

6. **Root Computation**: Compute root via streaming from MDBX using `StreamingTreeBuilder`

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

1. On startup, verifies persisted root via streaming (does not load full tree)
2. If a head exists, calls `ctx.notifications.set_with_head(head)`
3. reth's ExEx framework automatically backfills blocks from the persisted head to the current node head

**Fresh start (no persisted state):**
- Will process blocks as they arrive
- For existing synced nodes, use the migration tool first for faster initial sync

**Restart with persisted state:**
- Verifies root hash via streaming
- Automatically backfills any blocks missed during downtime

## Metrics

Prometheus-compatible metrics are exposed:

| Metric | Type | Description |
|--------|------|-------------|
| `ubt_exex_blocks_processed_total` | Counter | Total blocks processed |
| `ubt_exex_entries_per_block` | Histogram | State changes per block |
| `ubt_exex_stems_total` | Gauge | Total stems in tree |
| `ubt_exex_root_computation_seconds` | Histogram | Root hash computation time |
| `ubt_exex_persistence_seconds` | Histogram | MDBX write time |
| `ubt_exex_dirty_stems` | Gauge | Pending stems in overlay |
| `ubt_exex_reverts_total` | Counter | Revert operations |

## Troubleshooting

### Root verification failed on startup

If you see "Streaming root verification failed", the database may be inconsistent. Options:
- Delete the `ubt/` directory and rebuild from scratch
- Check for incomplete shutdown (use graceful shutdown with SIGTERM)

### Slow root computation

Root is computed via streaming on each flush. To reduce frequency:
- Increase `UBT_FLUSH_INTERVAL` (e.g., 10-100 blocks)
- Trade-off: larger overlay memory between flushes

### Reorg handling

Deltas are stored per-block for reorg support:
- Default retention: 256 blocks
- Increase `UBT_DELTA_RETENTION` for deeper reorg support
- If reorg exceeds retention, warning is logged and state may be inconsistent

## Integration with reth-ubt-migration

For existing nodes, first run the migration tool to build the initial UBT:

```bash
# 1. Run migration to build initial UBT state
cd ../reth-ubt-migration
cargo run --release -- --datadir ~/.local/share/reth/sepolia

# 2. Run reth with UBT ExEx to maintain state going forward
cd ../reth-ubt-exex
./target/release/reth-ubt node --chain sepolia
```

## Documentation

- [Architecture](docs/architecture.md) - System design and data flow
- [Changelog](CHANGELOG.md) - Version history

## Roadmap

| Status | Description |
|--------|-------------|
| Done | Deferred root computation, batch persistence, reorg handling |
| Done | MDBX-backed reads, streaming root computation |
| Open | Incremental root updates (requires ubt crate changes) |
| Open | Parallel hashing |

See [GitHub Issues](https://github.com/igor53627/reth-ubt-exex/issues) for details.

## Dependencies

- [ubt](https://github.com/igor53627/ubt-rs) - Unified Binary Tree implementation
- [reth](https://github.com/paradigmxyz/reth) - Ethereum execution client

## License

MIT OR Apache-2.0
