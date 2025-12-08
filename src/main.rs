//! reth-ubt-exex: ExEx plugin for maintaining UBT state alongside reth
//!
//! This plugin receives block notifications from reth and maintains a Unified Binary Tree
//! (EIP-7864) in parallel with the normal MPT state.
//!
//! Run with:
//! ```sh
//! cargo run --release -- node --chain sepolia
//! ```

use reth_ethereum::{node::EthereumNode, cli::Cli};

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
