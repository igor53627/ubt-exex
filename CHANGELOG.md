# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Performance
- Deferred root hash computation (#1) - `rebuild_root()` now called once per `root_hash()` instead of on every insert
  - Reduces per-block CPU from O(N * S log S) to O(S log S) where N=entries, S=stems
  - `root_hash()` now takes `&mut self` (breaking API change in ubt crate)
- Streaming root verification at startup (#6) - uses StreamingTreeBuilder for memory-efficient verification
- MDBX-backed read path (#5) - overlay-aware getters for proof generation without full tree
- Batch persistence across blocks (#2) - MDBX writes batched by `UBT_FLUSH_INTERVAL` env var
  - Default interval=1 maintains current behavior
  - Higher values reduce I/O when same stems modified across consecutive blocks

### Fixed
- Proper reorg handling with per-block deltas (#3)
  - Stores old values before applying state changes
  - Reverts apply deltas in LIFO order (newest block first, within-block reversed)
  - Deltas for persisted blocks preserved for crash recovery

### Added
- `iter_entries_sorted()` for streaming MDBX iteration
- `verify_root_streaming()` helper function
- `get_stem()` and `get_value()` overlay-aware getters
- Initial ExEx implementation with MDBX persistence
- Backfill support via `set_with_head()`
- State extraction from `BundleState` (accounts, storage, code)

### Known Issues
- Full tree still loaded into memory at startup (streaming is verification only)

## [0.1.0] - 2024-12-08

### Added
- Initial release
- UBT ExEx plugin for reth
- MDBX persistence for stem nodes
- Basic account/storage/code tracking
- Integration with `ubt` crate
