use std::{collections::HashMap, ops::RangeInclusive};

use alloy::{
    contract::Error as ContractError,
    primitives::BlockNumber,
    providers::Provider,
    rpc::types::Header,
    transports::{TransportError, TransportResult},
};

use mk1_chainio::taiko::{anchor::TaikoAnchorError, inbox::TaikoInbox};
use mk1_clients::execution::{ExecutionClient, TaikoExecutionClient};
use mk1_primitives::{
    taiko::{
        batch::{Batch, OrderedBatches},
        batch_map::{BatchMap, ReorgReason, ReorgReasons, Retimestamped},
        block::{ContiguousAnchoredBlocks, GapError},
    },
    time::current_timestamp_seconds,
};
use tracing::{error, info};

use crate::{
    config::RuntimeConfig, helpers::TaikoBlockExt, metrics::DriverMetrics, status::DriverStatus,
};

mod soft_blocks;
use soft_blocks::SoftBlocksDownloader;

/// Errors that can occur during the sync process.
#[derive(Debug, thiserror::Error)]
pub enum SyncerError {
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("Contract error: {0}")]
    Contract(#[from] ContractError),
    #[error("Anchor error: {0}")]
    Anchor(#[from] TaikoAnchorError),
    #[error("L1 block {0} not found")]
    L1BlockNotFound(BlockNumber),
    #[error("L2 block {0} not found")]
    L2BlockNotFound(BlockNumber),
    #[error("Gap error: {0}")]
    Gap(GapError),
}

/// The syncer is responsible for making sure that the L2 chain history is consistent with
/// what should be posted on L1. In particular, there are two main events that trigger a sync:
///
/// 1. on startup, to load the unsafe state and check if we need to post any batches before starting
///    to sequence new blocks.
/// 2. on the beginning of a new inclusion window, to check if the sequencer in the previous window
///    has correctly posted their batches or not. If not, we'll need to do it ourselves.
#[derive(Debug)]
pub(crate) struct Syncer {
    cfg: RuntimeConfig,
    l1_el: ExecutionClient,
    l2_el: TaikoExecutionClient,
    taiko_inbox: TaikoInbox,
    downloader: SoftBlocksDownloader,
}

impl Syncer {
    /// Creates a new [Sync] instance.
    pub(crate) async fn new(cfg: RuntimeConfig) -> TransportResult<Self> {
        let l1_el = ExecutionClient::new(cfg.l1.el_url.clone(), cfg.l1.el_ws_url.clone()).await?;
        let l2_el = ExecutionClient::new(cfg.l2.el_url.clone(), cfg.l2.el_ws_url.clone()).await?;

        let taiko_inbox = TaikoInbox::new(
            cfg.l1.el_url.clone(),
            cfg.contracts.taiko_inbox,
            cfg.operator.private_key.clone(),
        );

        let downloader = SoftBlocksDownloader::new(l1_el.clone(), l2_el.clone());

        Ok(Self { cfg, l1_el, l2_el, taiko_inbox, downloader })
    }

    /// Download the provided range of anchored blocks.
    pub(crate) async fn download_anchored_blocks(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> Result<ContiguousAnchoredBlocks, SyncerError> {
        self.downloader.download(range).await
    }

    /// Performs a sync routine, fetching soft blocks and creating batches.
    ///
    /// Returns a tuple containing:
    /// * The ordered batches created from the soft blocks.
    /// * The list of reasons for which posting these batches will cause a reorg, if any.
    /// * A map of block numbers to base fee. That can be used when reposting blocks to the p2p
    pub(crate) async fn sync(
        &self,
        status: DriverStatus,
    ) -> Result<(OrderedBatches, ReorgReasons, HashMap<BlockNumber, u64>), SyncerError> {
        let l2_head = self.l2_el.get_block_number().await?;
        let l2_head_l1_origin = self.taiko_inbox.get_head_l1_origin().await?;

        let l1_head = self.l1_el.get_block_number().await?;
        let l1_anchor_num = l1_head.saturating_sub(self.cfg.preconf.anchor_block_lag);
        let l1_anchor = self.l1_el.get_header(Some(l1_anchor_num)).await?;

        let settings = self.cfg.batch_settings();

        if l2_head <= l2_head_l1_origin {
            info!("🎯 There are no soft blocks, starting with an empty outstanding batch");
            return Ok((
                OrderedBatches::new_unchecked(vec![Batch::new(l1_anchor, settings)]),
                ReorgReasons::none(),
                HashMap::new(),
            ));
        }

        // The range of soft blocks that are not yet included in L1 batches.
        let range = l2_head_l1_origin + 1..=l2_head;

        match status {
            DriverStatus::Inactive => {
                // If we are inactive, we don't need to process any unsafe blocks, and
                // we can start with an empty outstanding batch.
                info!("🎯 Driver is inactive, starting with an empty outstanding batch");
                Ok((
                    OrderedBatches::new_unchecked(vec![Batch::new(l1_anchor, settings)]),
                    ReorgReasons::none(),
                    HashMap::new(),
                ))
            }
            DriverStatus::SequencingOnly | DriverStatus::HandoverWait { .. } => {
                // If we are in our sequencing-only window, it's ok to ignore all the unsafe blocks,
                // as they will be processed once we enter the inclusion window anyway.
                info!("🎯 Driver is in sequencing-only mode, ignoring unsafe blocks");

                let blocks = self.downloader.download(range).await?;
                let basefee_map = blocks.basefee_map();
                let batch = blocks.into_outstanding(l1_anchor, settings);
                Ok((OrderedBatches::new_unchecked(vec![batch]), ReorgReasons::none(), basefee_map))
            }
            DriverStatus::InclusionOnly | DriverStatus::Active => {
                info!("🎯 Driver can propose batches, creating them from all soft blocks");

                let blocks = self.downloader.download(range).await?;
                let basefee_map = blocks.basefee_map();
                let (batches, reorg_reasons) = self.create_batches(blocks, &l1_anchor).await?;
                Ok((batches, reorg_reasons, basefee_map))
            }
        }
    }

    /// Returns a list of batches ordered by their anchor block id and their proposer.
    pub(crate) async fn create_batches(
        &self,
        blocks: ContiguousAnchoredBlocks,
        safe_l1_anchor: &Header,
    ) -> Result<(OrderedBatches, ReorgReasons), SyncerError> {
        // Make sure the safe anchor is effectively the highest it can be: Max(safe, unsafe)
        let highest_unsafe = self.l2_el.get_block(None, true).await?;
        let guaranteed_safe_anchor = if highest_unsafe.anchor_block_id()? > safe_l1_anchor.number {
            &highest_unsafe.header
        } else {
            safe_l1_anchor
        };

        let batch_map = self.create_batch_map(blocks, guaranteed_safe_anchor).await?;

        // Track the reorg reasons for metrics
        for reason in batch_map.reorg_reasons().iter() {
            match reason {
                ReorgReason::Reanchor { expired_anchor, new_anchor } => {
                    DriverMetrics::increment_batches_reanchored(
                        batch_map.count(),
                        *expired_anchor,
                        *new_anchor,
                    );
                }
                ReorgReason::Retimestamp { max_ts, .. } => {
                    DriverMetrics::increment_batches_retimestamped(*max_ts);
                }
            }
        }

        let reorg_reasons = batch_map.reorg_reasons().clone();
        Ok((batch_map.into_vec(), reorg_reasons))
    }

    /// Creates batches of unsafe blocks, grouping them by their anchor block.
    ///
    /// Returns a map of anchor block IDs to their corresponding batches.
    /// Batches with the same anchor block are ordered by increasing block number.
    async fn create_batch_map(
        &self,
        blocks: ContiguousAnchoredBlocks,
        safe_l1_anchor: &Header,
    ) -> Result<BatchMap<Retimestamped>, SyncerError> {
        let settings = self.cfg.batch_settings();

        // Skip all network calls if there are no soft blocks
        if blocks.is_empty() {
            return Ok(BatchMap::default())
        }

        // Assumption: unsafe blocks are older than outstanding ones. We can start from
        // the oldest block and check if each batch needs to be reorged and keep track of the
        // minimum anchor block id of each subsequent batch.

        // get the raw batch maps
        let batch_map = BatchMap::new(blocks, settings);

        let l1_head_id = self.l1_el.get_block_number().await?;
        let min_proposable_anchor_id = l1_head_id + self.cfg.preconf.anchor_max_height_buffer -
            self.cfg.pacaya_config.maxAnchorHeightOffset;
        let min_proposable_anchor = self.l1_el.get_header(Some(min_proposable_anchor_id)).await?;

        let max_ts = current_timestamp_seconds();
        let min_proposable_ts = max_ts + self.cfg.default_proposal_time_buffer() -
            self.cfg.max_proposable_block_age_secs();

        let batch_map = batch_map
            .ensure_unique_coinbase()
            .reanchor(min_proposable_anchor.number, safe_l1_anchor, settings)
            .retimestamp(min_proposable_ts, max_ts);

        Ok(batch_map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::types::Block;
    use alloy_primitives::Address;
    use mk1_primitives::{
        taiko::batch::{BatchSettings, PreconfirmedBlock},
        time::Timestamp,
    };

    /// Helper function to create a test header with just the essential fields
    fn create_test_header(number: u64, timestamp: u64) -> Header {
        Header {
            inner: alloy::consensus::Header { number, timestamp, ..Default::default() },
            ..Default::default()
        }
    }

    /// Helper function to create a test block with just the essential fields
    fn create_test_block(number: u64, timestamp: u64, beneficiary: Address) -> Block {
        Block {
            header: Header {
                inner: alloy::consensus::Header {
                    number,
                    timestamp,
                    beneficiary,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    const BENEFICIARY: Address = Address::new([1; 20]);

    #[test]
    fn test_batch_map_into_vec_ordering() {
        let mut batch_map = BatchMap::<()>::default();
        let cap = BatchSettings {
            capacity: 10,
            target_compressed_size: 1000,
            max_raw_size: 1000,
            default_coinbase: BENEFICIARY,
            max_anchor_height_offset: 64,
            anchor_max_height_buffer: 6,
        };

        // Create batches with different anchor blocks and block numbers
        // Anchor block 2, block numbers 5 and 6
        let anchor_2 = create_test_header(2, 1000);
        let block_5 = create_test_block(5, 1000, BENEFICIARY);
        let block_6 = create_test_block(6, 1001, BENEFICIARY);
        let preconfirmed_5 = PreconfirmedBlock::from_block(block_5);
        let preconfirmed_6 = PreconfirmedBlock::from_block(block_6);
        let batch_2_5 = Batch::from_blocks_unchecked(anchor_2.clone(), cap, vec![preconfirmed_5]);
        let batch_2_6 = Batch::from_blocks_unchecked(anchor_2.clone(), cap, vec![preconfirmed_6]);
        batch_map.insert(2, vec![batch_2_5, batch_2_6]); // Note: order here is important

        // Anchor block 1, block numbers 3 and 4
        let anchor_1 = create_test_header(1, 900);
        let block_3 = create_test_block(3, 900, BENEFICIARY);
        let block_4 = create_test_block(4, 901, BENEFICIARY);
        let preconfirmed_3 = PreconfirmedBlock::from_block(block_3);
        let preconfirmed_4 = PreconfirmedBlock::from_block(block_4);
        let batch_1_3 = Batch::from_blocks_unchecked(anchor_1.clone(), cap, vec![preconfirmed_3]);
        let batch_1_4 = Batch::from_blocks_unchecked(anchor_1.clone(), cap, vec![preconfirmed_4]);
        batch_map.insert(1, vec![batch_1_3, batch_1_4]); // Note: order here is important

        let batch_map = batch_map
            .ensure_unique_coinbase()
            .reanchor(anchor_1.number, &anchor_2, cap)
            .retimestamp(0, Timestamp::MAX);

        // Convert to vec and verify ordering
        let batches = batch_map.into_vec();
        assert_eq!(batches.len(), 4);

        // Verify ordering by anchor block first
        assert_eq!(batches[0].anchor_id(), 1); // First anchor block
        assert_eq!(batches[2].anchor_id(), 2); // Second anchor block

        // Verify ordering by block number within same anchor block
        assert_eq!(batches[0].blocks().first().unwrap().number(), 3); // First block in anchor 1
        assert_eq!(batches[1].blocks().first().unwrap().number(), 4); // Second block in anchor 1
        assert_eq!(batches[2].blocks().first().unwrap().number(), 5); // First block in anchor 2
        assert_eq!(batches[3].blocks().first().unwrap().number(), 6); // Second block in anchor 2
    }

    // Note: background compression requires a tokio runtime
    #[tokio::test]
    async fn test_batch_map_reanchor_below_minimum() {
        let mut batch_map = BatchMap::default();
        let settings = BatchSettings {
            capacity: 2,
            target_compressed_size: 1000,
            max_raw_size: 1000,
            default_coinbase: BENEFICIARY,
            max_anchor_height_offset: 64,
            anchor_max_height_buffer: 6,
        };

        // Anchors: Old (1, 3), Safe (5), Min Proposable (4)
        let old_anchor_1 = create_test_header(1, 900);
        let old_anchor_3 = create_test_header(3, 1100);
        let safe_anchor = create_test_header(5, 1300);
        let min_proposable = 4;

        // Blocks anchored to old blocks
        let block_10 = create_test_block(10, 901, BENEFICIARY); // Anchor 1
        let block_11 = create_test_block(11, 902, BENEFICIARY); // Anchor 1
        let block_12 = create_test_block(12, 1101, BENEFICIARY); // Anchor 3
        let block_13 = create_test_block(13, 1102, BENEFICIARY); // Anchor 3
        let block_14 = create_test_block(14, 1103, BENEFICIARY); // Anchor 3

        let p_block_10 = PreconfirmedBlock::from_block(block_10);
        let p_block_11 = PreconfirmedBlock::from_block(block_11);
        let p_block_12 = PreconfirmedBlock::from_block(block_12);
        let p_block_13 = PreconfirmedBlock::from_block(block_13);
        let p_block_14 = PreconfirmedBlock::from_block(block_14);

        // Create initial batches (capacity 2)
        // Batch 1 (Anchor 1): Blocks 10, 11
        let batch_1 = Batch::from_blocks_unchecked(
            old_anchor_1.clone(),
            settings,
            vec![p_block_10, p_block_11],
        );
        batch_map.insert(old_anchor_1.number, vec![batch_1]);

        // Batch 2 (Anchor 3): Blocks 12, 13
        let batch_2 = Batch::from_blocks_unchecked(
            old_anchor_3.clone(),
            settings,
            vec![p_block_12, p_block_13],
        );
        // Batch 3 (Anchor 3): Block 14
        let batch_3 =
            Batch::from_blocks_unchecked(old_anchor_3.clone(), settings, vec![p_block_14]);
        batch_map.insert(old_anchor_3.number, vec![batch_3, batch_2]); // Inserted newest first

        let batch_map = batch_map.ensure_unique_coinbase();

        // Verify should_reanchor is true (min anchor 1 < min_proposable 4)
        assert!(batch_map.should_reanchor(min_proposable));

        // Reanchor to the safe anchor (5)
        let batch_map = batch_map.reanchor(min_proposable, &safe_anchor, settings);

        // Verify should_reanchor is now false (min anchor 5 >= min_proposable 4)
        assert!(!batch_map.should_reanchor(min_proposable));

        // Check the state after reanchoring
        assert_eq!(batch_map.len(), 1, "Should only have one entry for the safe anchor");
        assert!(batch_map.contains_key(&safe_anchor.number));

        let reanchored_batches = batch_map.get(&safe_anchor.number).unwrap();
        assert_eq!(reanchored_batches.len(), 3, "Should have 3 batches due to capacity limit");

        for reanchored_batch in reanchored_batches {
            assert_eq!(reanchored_batch.anchor_id(), safe_anchor.number);
        }

        // Batches are newest first after reanchor
        // Batch 1: Blocks 10, 11
        // Batch 2: Blocks 12, 13
        // Batch 3: Block 14 -> This will be the last batch in the vec (newest)

        // Check newest batch (batch 3 after reanchor)
        let newest_batch = &reanchored_batches[0];
        assert_eq!(newest_batch.anchor_id(), safe_anchor.number);
        assert_eq!(newest_batch.blocks().len(), 2);
        assert_eq!(newest_batch.blocks()[0].number(), 10);
        assert_eq!(newest_batch.blocks()[1].number(), 11);

        // Check middle batch (batch 2 after reanchor)
        let middle_batch = &reanchored_batches[1];
        assert_eq!(middle_batch.anchor_id(), safe_anchor.number);
        assert_eq!(middle_batch.blocks().len(), 2);
        assert_eq!(middle_batch.blocks()[0].number(), 12);
        assert_eq!(middle_batch.blocks()[1].number(), 13);

        // Check oldest batch (batch 1 after reanchor)
        let oldest_batch = &reanchored_batches[2];
        assert_eq!(oldest_batch.anchor_id(), safe_anchor.number);
        assert_eq!(oldest_batch.blocks().len(), 1);
        assert_eq!(oldest_batch.blocks()[0].number(), 14);

        // Check total blocks count
        let total_blocks: usize = reanchored_batches.iter().map(|b| b.blocks().len()).sum();
        assert_eq!(total_blocks, 5);
    }
}
