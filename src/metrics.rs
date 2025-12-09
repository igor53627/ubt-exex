//! Metrics for UBT ExEx.
//!
//! Exposes Prometheus-compatible metrics for monitoring UBT performance.
//! Uses the `metrics` crate with static metric names.

use metrics::{counter, gauge, histogram};

const BLOCKS_PROCESSED_TOTAL: &str = "ubt_exex_blocks_processed_total";
const ENTRIES_PER_BLOCK: &str = "ubt_exex_entries_per_block";
const STEMS_TOTAL: &str = "ubt_exex_stems_total";
const LAST_BLOCK_NUMBER: &str = "ubt_exex_last_block_number";

const ROOT_COMPUTATION_SECONDS: &str = "ubt_exex_root_computation_seconds";
const PERSISTENCE_SECONDS: &str = "ubt_exex_persistence_seconds";
const STEMS_PERSISTED: &str = "ubt_exex_stems_persisted";

const DIRTY_STEMS: &str = "ubt_exex_dirty_stems";
const REVERTS_TOTAL: &str = "ubt_exex_reverts_total";
const REVERT_BLOCKS: &str = "ubt_exex_revert_blocks";
const REVERT_ENTRIES: &str = "ubt_exex_revert_entries";

/// Record a block being processed.
pub fn record_block_processed(block_number: u64, entries: usize, stems: usize) {
    counter!(BLOCKS_PROCESSED_TOTAL).increment(1);
    histogram!(ENTRIES_PER_BLOCK).record(entries as f64);
    gauge!(STEMS_TOTAL).set(stems as f64);
    gauge!(LAST_BLOCK_NUMBER).set(block_number as f64);
}

/// Record root computation time.
pub fn record_root_computation(duration_secs: f64) {
    histogram!(ROOT_COMPUTATION_SECONDS).record(duration_secs);
}

/// Record persistence operation time.
pub fn record_persistence(duration_secs: f64, stems_written: usize) {
    histogram!(PERSISTENCE_SECONDS).record(duration_secs);
    histogram!(STEMS_PERSISTED).record(stems_written as f64);
}

/// Record dirty stems gauge.
pub fn record_dirty_stems(count: usize) {
    gauge!(DIRTY_STEMS).set(count as f64);
}

/// Record a revert operation.
pub fn record_revert(blocks_reverted: usize, entries_reverted: usize) {
    counter!(REVERTS_TOTAL).increment(1);
    histogram!(REVERT_BLOCKS).record(blocks_reverted as f64);
    histogram!(REVERT_ENTRIES).record(entries_reverted as f64);
}
