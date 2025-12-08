# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial ExEx implementation with MDBX persistence
- Backfill support via `set_with_head()`
- State extraction from `BundleState` (accounts, storage, code)

### Known Issues
- Reorgs not properly handled (#3)
- Full tree loaded into memory at startup (#6)
- Root hash recomputed on every insert (#1)

## [0.1.0] - 2024-12-08

### Added
- Initial release
- UBT ExEx plugin for reth
- MDBX persistence for stem nodes
- Basic account/storage/code tracking
- Integration with `ubt` crate
