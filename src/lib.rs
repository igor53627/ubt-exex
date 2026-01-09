//! ubt-exex library exports.
//!
//! This exposes internal modules for reuse in benchmarks and integrations.

pub mod config;
pub mod error;
pub mod key_index;
pub mod mdbx;
pub mod metrics;
pub mod persistence;
pub mod pir_export;
pub mod rpc;
pub mod rpc_server;
pub mod ubt_exex;

pub use ubt_exex::ubt_exex;

#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod proptest_strategies;
