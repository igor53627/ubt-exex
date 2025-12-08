# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Performance
- Streaming root verification at startup (#6) - uses StreamingTreeBuilder for memory-efficient verification
- MDBX-backed read path (#5) - overlay-aware getters for proof generation without full tree

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
