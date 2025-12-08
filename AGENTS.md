# AGENTS.md - AI Agent Guidelines for reth-ubt-exex

## Project Overview

reth-ubt-exex is an Execution Extension (ExEx) plugin for reth that maintains EIP-7864 Unified Binary Tree state.

## Commands

```bash
# Build
cargo build --release

# Run tests
cargo test

# Run with reth (Sepolia)
RETH_DATA_DIR=/path/to/data ./target/release/reth-ubt node --chain sepolia

# Format
cargo +nightly fmt

# Lint
cargo clippy --all-features

# Generate docs
cargo doc --no-deps --document-private-items
```

## Project Structure

```
reth-ubt-exex/
├── src/
│   ├── main.rs          # Binary entry point, reth node setup
│   ├── ubt_exex.rs      # Core ExEx logic, state extraction
│   └── persistence.rs   # MDBX database operations
├── docs/
│   └── architecture.md  # System design and data flow
├── CHANGELOG.md         # Version history
├── README.md            # User-facing documentation
└── AGENTS.md            # This file
```

## Dependencies

- `ubt` crate at `../ubt` - Core UBT implementation
- `reth-ubt` at `../reth-ubt` - Local reth fork with path dependencies

## Documentation Best Practices

### Code-Level Documentation

1. **Public items**: Use `///` doc comments
   ```rust
   /// Computes the UBT root hash for the current state.
   /// 
   /// # Returns
   /// The 32-byte Blake3 hash of the tree root.
   pub fn root_hash(&self) -> B256 { ... }
   ```

2. **Module-level**: Use `//!` at top of files
   ```rust
   //! MDBX persistence for UBT state.
   //!
   //! Stores UBT stems and their values in MDBX tables.
   ```

3. **Examples in docs**: These become tests via `cargo test --doc`
   ```rust
   /// # Example
   /// ```
   /// let tree = UnifiedBinaryTree::new();
   /// assert!(tree.is_empty());
   /// ```
   ```

### Project-Level Documentation

| File | Purpose | Update Frequency |
|------|---------|------------------|
| `README.md` | Usage, installation, quick start | Major features |
| `CHANGELOG.md` | Version history, breaking changes | Every PR/issue |
| `docs/architecture.md` | System design, data flow | Architectural changes |
| `AGENTS.md` | AI agent instructions | When workflows change |

### After Closing Issues

1. **Update code docs**: Add/modify `///` comments for changed APIs
2. **Update CHANGELOG.md**: Add entry under `## [Unreleased]`
3. **Update architecture docs**: If design changed significantly
4. **Run doc check**: `cargo doc --no-deps` to catch broken links

### CHANGELOG Format

Follow [Keep a Changelog](https://keepachangelog.com/):

```markdown
## [Unreleased]

### Added
- New feature description (#issue-number)

### Changed
- Modified behavior description (#issue-number)

### Fixed
- Bug fix description (#issue-number)

### Performance
- Optimization description (#issue-number)
```

## GitHub Issues

Track optimization work at: https://github.com/igor53627/reth-ubt-exex/issues

Priority order:
- P0: #1 (Deferred Root Hash) - do first
- P1: #2 (Batch Persistence), #3 (Reorg Handling)
- P2: #4 (Incremental Root), #5 (MDBX-Backed Reads)
- P3: #6 (Streaming Loader), #7 (Parallel Hashing)

## Testing Guidelines

1. **Unit tests**: In same file with `#[cfg(test)]` module
2. **Integration tests**: In `tests/` directory (create if needed)
3. **Property tests**: Use `proptest` for randomized testing
4. **Differential tests**: Compare optimized vs reference implementation

## Performance Notes

- Sepolia state: ~2.77B entries, ~782GB RAM, ~100M stems
- Migration time: ~55 min on 1.1TB RAM server
- Target: Maintain state with < 100ms latency per block
