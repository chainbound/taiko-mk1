use std::time::Duration;

use alloy::{
    network::Ethereum,
    providers::Provider,
    rpc::types::{Block, SyncStatus},
};
use mk1_chainio::taiko::anchor::{TaikoAnchor, TaikoAnchorError, TaikoAnchorTxInput};
use mk1_clients::execution::{ExecutionClient, TaikoExecutionClient};
use tokio::sync::oneshot;
use tracing::{debug, info};

use crate::{config::RuntimeConfig, driver::DriverError, metrics::DriverMetrics};

pub(crate) trait TaikoBlockExt {
    /// Returns the anchor transaction from the block, if it exists and can be decoded.
    fn anchor_tx(&self) -> Result<TaikoAnchorTxInput, TaikoAnchorError>;

    /// Returns the anchor block ID from the block, if it exists and can be decoded.
    fn anchor_block_id(&self) -> Result<u64, TaikoAnchorError> {
        self.anchor_tx().map(|tx| tx.anchor_block_id)
    }
}

impl TaikoBlockExt for Block {
    fn anchor_tx(&self) -> Result<TaikoAnchorTxInput, TaikoAnchorError> {
        TaikoAnchor::decode_anchor_tx_from_full_block(self)
    }
}

/// Future that waits for the L1 and L2 execution clients to sync before resolving.
pub(crate) async fn wait_for_clients_sync(cfg: &RuntimeConfig) -> Result<(), DriverError> {
    let l1_el =
        ExecutionClient::<Ethereum>::new(cfg.l1.el_url.clone(), cfg.l1.el_ws_url.clone()).await?;
    let l2_el = TaikoExecutionClient::new(cfg.l2.el_url.clone(), cfg.l2.el_ws_url.clone()).await?;

    debug!("Waiting for L1 and L2 to sync...");
    loop {
        let (l1_syncing, l2_syncing) = tokio::try_join!(l1_el.syncing(), l2_el.syncing())?;

        match (&l1_syncing, &l2_syncing) {
            (SyncStatus::None, SyncStatus::None) => return Ok(()),
            (SyncStatus::None, SyncStatus::Info(info)) |
            (SyncStatus::Info(info), SyncStatus::None) => {
                // If only one chain is syncing, consider it synced if it's at genesis.
                // This is to avoid getting stuck in a loop when the highest block is zero.
                if info.highest_block.is_zero() {
                    return Ok(())
                }

                let chain = if matches!(l2_syncing, SyncStatus::Info(_)) { "L2" } else { "L1" };
                info!(
                    starting_block = %info.starting_block,
                    current_block = %info.current_block,
                    highest_block = %info.highest_block,
                    "{chain} is syncing..."
                );
            }
            (SyncStatus::Info(l1_info), SyncStatus::Info(l2_info)) => {
                for (chain, info) in [("L1", l1_info), ("L2", l2_info)] {
                    info!(
                        starting_block = %info.starting_block,
                        current_block = %info.current_block,
                        highest_block = %info.highest_block,
                        "{chain} is syncing..."
                    );
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Compute the batch size metrics in the background.
pub(crate) fn compute_batch_size_metrics(
    raw_size: usize,
    compressed_size_rx: oneshot::Receiver<usize>,
) {
    tokio::spawn(async move {
        if let Ok(compressed_size) = compressed_size_rx.await {
            let ratio = compressed_size as f64 / raw_size as f64;
            debug!(raw = raw_size, compressed = compressed_size, ratio, "Batch size");
            DriverMetrics::set_batch_size(raw_size, compressed_size as u32);
        }
    });
}
