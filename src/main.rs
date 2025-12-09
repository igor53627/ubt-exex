//! reth-ubt-exex: ExEx plugin for maintaining UBT state alongside reth
//!
//! This binary provides a reth node with an integrated Execution Extension (ExEx)
//! that maintains an EIP-7864 Unified Binary Tree in parallel with the normal MPT state.
//!
//! # Usage
//!
//! ```sh
//! # Run on Sepolia testnet
//! cargo run --release -- node --chain sepolia
//!
//! # With custom data directory
//! RETH_DATA_DIR=/path/to/data cargo run --release -- node --chain sepolia
//! ```
//!
//! # Configuration
//!
//! Environment variables:
//! - `RETH_DATA_DIR`: Base directory for data storage (default: current directory)
//! - `UBT_FLUSH_INTERVAL`: Blocks between MDBX flushes (default: 1)
//! - `UBT_DELTA_RETENTION`: Blocks to retain deltas for reorgs (default: 256)

use reth_ethereum::{node::EthereumNode, cli::Cli};

mod config;
mod error;
mod metrics;
mod persistence;
mod ubt_exex;

use ubt_exex::ubt_exex;

fn main() -> eyre::Result<()> {
    Cli::parse_args().run(|builder, _| {
        Box::pin(async move {
            let handle = builder
                .node(EthereumNode::default())
                .install_exex("ubt", |ctx| async move { Ok(ubt_exex(ctx)) })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        })
    })
}
