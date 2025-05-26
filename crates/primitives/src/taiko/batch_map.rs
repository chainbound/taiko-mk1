use std::{collections::BTreeMap, marker::PhantomData};

use alloy::rpc::types::Header;
use alloy_primitives::BlockNumber;
use derive_more::derive::{Deref, DerefMut, From, Into};
use tracing::{debug, warn};

use crate::{
    summary::Summary,
    taiko::{
        batch::{AddBlockResult, Batch, BatchSettings, OrderedBatches, PreconfirmedBlock},
        block::ContiguousAnchoredBlocks,
    },
    time::Timestamp,
};

// The list of states that a batch map can be in, where one state implies the previous one:
// () -> CoinbaseChecked -> Anchored -> Retimestamped

/// A marker struct that indicates a batch map where each batch has a single coinbase for all
/// its L2 blocks.
#[derive(Debug, Default, Clone)]
pub struct CoinbaseChecked;
/// A marker struct that indicates a batch map where each batch has a single, sufficiently recent
/// anchor block.
#[derive(Debug, Default, Clone)]
pub struct Anchored;
/// A marker struct that indicates a batch map where each batch has its timestamp reset to
/// the current timestamp if it is outside of the specified valid range.
#[derive(Debug, Default, Clone)]
pub struct Retimestamped;

/// A map of anchor block IDs to their corresponding list of batches.
///
/// After a batch is in the [`CoinbaseChecked`] status, the following holds:
/// 1. All batches are grouped by anchor id
/// 2. Batches with the same anchor don't have mixed coinbase
/// 3. Thanks to *, batches with the same anchor are ordered by block number, increasing.
///
/// In case re-anchoring or re-timestamping takes place, these properties are still preserved
///
/// * taiko-driver enforces that only the correct sequencer can post during a sequencing window.
#[derive(Debug, Clone, Default, Deref, DerefMut)]
pub struct BatchMap<S = ()> {
    #[deref]
    #[deref_mut]
    /// The inner map of anchor block IDs to their corresponding list of batches.
    inner: BTreeMap<BlockNumber, Vec<Batch>>,
    /// A list of reorg reasons for the batch map. If empty, batches in the map
    /// can be posted without causing any L2 reorgs.
    reorg_reasons: ReorgReasons,
    /// A marker to indicate the state of the batch map.
    _phantom: PhantomData<S>,
}

impl BatchMap {
    /// Create a new batch map from a list of blocks and an L1 execution client.
    pub fn new(anchored_blocks: ContiguousAnchoredBlocks, settings: BatchSettings) -> Self {
        let mut batch_map = Self::default();

        if anchored_blocks.is_empty() {
            return batch_map;
        }

        for anchored_block in anchored_blocks {
            let (block, anchor) = anchored_block.into_parts();
            let preconfirmed = PreconfirmedBlock::from_block(block);
            let anchor_block_id = anchor.inner.number;

            if let Some(batches_by_anchor) = batch_map.get_mut(&anchor_block_id) {
                let mut current_batch = batches_by_anchor.pop().expect("map entry exists");

                match current_batch.add_block_and_reset_if_full(preconfirmed, &anchor) {
                    AddBlockResult::Added { .. } => {
                        // No-Op. We'll re-add the current batch to the list and continue.
                        batches_by_anchor.push(current_batch);
                    }
                    AddBlockResult::Full { batch, reason } => {
                        debug!(?reason, "Soft batch is full: {}", batch.summary());
                        // Now `current_batch` contains the newly added block, so it is a
                        // "new_batch", while `batch` contains the previous ones.
                        let new_batch = current_batch;
                        batches_by_anchor.push(batch);
                        batches_by_anchor.push(new_batch);
                    }
                }
            } else {
                let batch = Batch::from_blocks_unchecked(
                    anchor,
                    // NOTE: try to keep the original coinbase
                    BatchSettings { default_coinbase: preconfirmed.coinbase(), ..settings },
                    vec![preconfirmed],
                );
                batch_map.insert(anchor_block_id, vec![batch]);
            }
        }

        for batches_by_anchor in batch_map.values_mut() {
            // Sort the batches their block number. Above we check that `anchored_blocks`
            // is not empty so we can safely expect to find the first block.
            batches_by_anchor
                .sort_by_key(|b| b.blocks().first().expect("non-empty batches").number());
        }

        batch_map
    }

    /// Ensures that each batch has a unique coinbase across all its blocks. If a batch has multiple
    /// coinbases, it is then split them into multiple batches.
    pub fn ensure_unique_coinbase(mut self) -> BatchMap<CoinbaseChecked> {
        for batches in self.values_mut() {
            let mut new_batches = Vec::new();
            for batch in batches.iter_mut() {
                new_batches.extend(batch.clone().split_by_coinbase());
            }

            *batches = new_batches;
        }

        BatchMap::from(self.inner)
    }
}

impl BatchMap<CoinbaseChecked> {
    /// If needed, reanchor all batches to the provided safe anchor block
    ///
    /// Re-anchoring will cause an L2 reorg, because preconfirmed blocks had a different anchor
    /// block.
    pub fn reanchor(
        mut self,
        min_proposable_anchor: BlockNumber,
        safe_l1_anchor: &Header,
        settings: BatchSettings,
    ) -> BatchMap<Anchored> {
        let Some(min_anchor) = self.min_anchor() else {
            return BatchMap::from(self.inner);
        };
        if !self.should_reanchor(min_proposable_anchor) {
            return BatchMap::from(self.inner);
        }

        warn!(
            min_proposable = min_proposable_anchor,
            oldest_anchor = min_anchor,
            "Reanchoring all batches to the safe anchor"
        );

        // When re-anchoring we will set to coinbase to the running sequencer,
        // so all batches in the new map will have the same coinbase.
        let mut new = BatchMap::default().ensure_unique_coinbase();

        // 1. Dismantle the existing batches back into blocks
        let mut blocks = std::mem::take(&mut self.inner)
            .into_values()
            .flatten()
            .flat_map(|b| b.into_blocks())
            .collect::<Vec<_>>();

        // Sort by number to ensure the blocks are in order (sanity check)
        blocks.sort_by_key(|b| b.number());

        // 2. Reassemble batches with the safe anchor and new coinbase. This will cause an L2 reorg
        //    because preconfirmed blocks had a different anchor block.
        let mut cursor = Batch::new(safe_l1_anchor.clone(), settings);
        for block in blocks {
            match cursor.add_block_and_reset_if_full(block, safe_l1_anchor) {
                AddBlockResult::Added { .. } => { /* pass-through */ }
                AddBlockResult::Full { batch, reason } => {
                    debug!(?reason, "Reanchored batch is full: {}", batch.summary());
                    new.inner.entry(batch.anchor_id()).or_insert_with(Vec::new).push(batch);
                }
            }
        }

        // 3. Finally insert the cursor with the last blocks
        new.inner.entry(safe_l1_anchor.number).or_insert_with(Vec::new).push(cursor);

        // 4. Replace the old map with the new one
        self.inner = new.inner;

        let reorg_reason =
            ReorgReason::Reanchor { expired_anchor: min_anchor, new_anchor: safe_l1_anchor.number };

        BatchMap {
            inner: self.inner,
            reorg_reasons: ReorgReasons::from(vec![reorg_reason]),
            _phantom: PhantomData,
        }
    }
}

impl BatchMap<Anchored> {
    /// Resets the timestamps of all the batches in the map to the maximum timestamp if they
    /// are outside of the specified valid range.
    pub fn retimestamp(mut self, min_ts: Timestamp, max_ts: Timestamp) -> BatchMap<Retimestamped> {
        let mut should_retimestamp = false;

        for batches_by_anchor_id in self.inner.values_mut() {
            for batch in batches_by_anchor_id {
                // Only track the first batch that needs retimestamping, then apply the new
                // timestamp to all blocks in the batch and to all batches after it as well.
                if !should_retimestamp {
                    for block in batch.blocks() {
                        if block.timestamp() < min_ts {
                            debug!("Block {} timestamp is below min {}", block.summary(), min_ts);
                            should_retimestamp = true;
                        } else if block.timestamp() > max_ts {
                            debug!("Block {} timestamp is above max {}", block.summary(), max_ts);
                            should_retimestamp = true;
                        }
                    }
                }

                if should_retimestamp {
                    debug!(new = max_ts, "Resetting all timestamps for batch: {}", batch.summary());
                    batch.set_all_blocks_timestamp(max_ts);
                }
            }
        }

        if should_retimestamp {
            self.reorg_reasons.push(ReorgReason::Retimestamp { min_ts, max_ts })
        }

        BatchMap { inner: self.inner, reorg_reasons: self.reorg_reasons, _phantom: PhantomData }
    }
}

impl BatchMap<Retimestamped> {
    /// Returns a vector containing all the batches ordered by their
    /// anchor block number, consuming the map.
    pub fn into_vec(self) -> OrderedBatches {
        let batches = self.inner.into_values().flatten().collect::<Vec<_>>();
        // NOTE: it is safe here to use `new_unchecked` because a batch map is
        // created from a list of [`ContiguousAnchoredBlocks`].
        OrderedBatches::new_unchecked(batches)
    }
}

impl<S> BatchMap<S> {
    /// Returns the minimum anchor block number in the map, or `None` if the map is empty.
    pub fn min_anchor(&self) -> Option<BlockNumber> {
        self.keys().min().copied()
    }

    /// Returns `true` if any batch in the map should be reanchored, i.e. if the minimum anchor
    /// block number is less than the minimum proposable anchor block number.
    pub fn should_reanchor(&self, min_proposable_anchor: BlockNumber) -> bool {
        let Some(key) = self.min_anchor() else {
            return false;
        };

        let Some(min) = self.get(&key).expect("key exists").first() else {
            return false;
        };

        let min = min.anchor().number;

        min < min_proposable_anchor
    }

    /// Returns the total number of batches in the map.
    pub fn count(&self) -> u64 {
        self.inner.values().map(|v| v.len() as u64).sum()
    }

    /// Returns the list of reorg reasons for the batch map.
    #[allow(clippy::missing_const_for_fn)]
    pub fn reorg_reasons(&self) -> &ReorgReasons {
        &self.reorg_reasons
    }
}

impl<S> From<BTreeMap<BlockNumber, Vec<Batch>>> for BatchMap<S> {
    fn from(inner: BTreeMap<BlockNumber, Vec<Batch>>) -> Self {
        Self { inner, reorg_reasons: ReorgReasons::none(), _phantom: PhantomData }
    }
}

/// The reason for which a batch might cause an L2 reorg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum ReorgReason {
    Reanchor { expired_anchor: BlockNumber, new_anchor: BlockNumber },
    Retimestamp { min_ts: Timestamp, max_ts: Timestamp },
}

/// A list of [`ReorgReason`] for which a batch might cause an L2
#[derive(Debug, Clone, Default, Deref, DerefMut, Into, From)]
pub struct ReorgReasons(Vec<ReorgReason>);

impl ReorgReasons {
    /// Creates an empty list of reorg reasons.
    pub fn none() -> Self {
        Self::default()
    }

    /// Returns `true` if the list of reorg reasons contains reanchoring reasons.
    pub fn only_reanchoring(&self) -> bool {
        self.0.iter().all(|r| matches!(r, ReorgReason::Reanchor { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::types::Block;
    use alloy_primitives::{Address, B256, BlockNumber, address};
    use std::{
        sync::LazyLock,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::taiko::{
        batch::BatchSettings, block::AnchoredBlock, constants::TAIKO_GOLDEN_TOUCH_ACCOUNT,
    };

    type BatchMapInitialized = BatchMap<()>;

    // --- Mock Data Helpers ---

    static TEST_SETTINGS: LazyLock<BatchSettings> = LazyLock::new(|| BatchSettings {
        capacity: 10,
        target_compressed_size: 100_000, // Arbitrary value for testing
        max_raw_size: 120_000,           // Arbitrary value for testing
        default_coinbase: TAIKO_GOLDEN_TOUCH_ACCOUNT, // Use a default constant
        max_anchor_height_offset: 64,
        anchor_max_height_buffer: 6,
    });

    const BENEFICIARY: Address = address!("0x00000000000000000aaa00000000000000000000");

    fn mock_header(number: BlockNumber, timestamp: u64) -> Header {
        Header {
            inner: alloy::consensus::Header { number, timestamp, ..Default::default() },
            hash: B256::new([0; 32]),
            ..Default::default()
        }
    }

    fn mock_block_header(number: BlockNumber, timestamp: u64, beneficiary: Address) -> Header {
        Header {
            inner: alloy::consensus::Header {
                number,
                timestamp,
                beneficiary,
                ..Default::default()
            },
            hash: B256::new([0; 32]),
            ..Default::default()
        }
    }

    fn mock_anchored_block(
        l2_num: BlockNumber,
        l2_ts: u64,
        l2_coinbase: Address,
        anchor_num: BlockNumber,
        anchor_ts: u64,
    ) -> AnchoredBlock {
        let anchor_header = mock_header(anchor_num, anchor_ts);
        let l2_full_block = Block {
            header: mock_block_header(l2_num, l2_ts, l2_coinbase),
            transactions: alloy::rpc::types::BlockTransactions::Hashes(vec![]),
            ..Default::default()
        };
        AnchoredBlock::new(l2_full_block, anchor_header)
    }

    fn current_timestamp() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    // --- Tests ---

    #[tokio::test]
    async fn test_new_batch_map() {
        let ts = current_timestamp();
        let anchor1_num = 10;
        let anchor2_num = 11;
        let blocks = vec![
            mock_anchored_block(100, ts, BENEFICIARY, anchor1_num, ts - 10),
            mock_anchored_block(101, ts + 1, BENEFICIARY, anchor1_num, ts - 10),
            mock_anchored_block(102, ts + 2, BENEFICIARY, anchor2_num, ts),
        ];

        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), *TEST_SETTINGS);

        assert_eq!(batch_map.len(), 2); // Two anchor blocks
        assert!(batch_map.contains_key(&anchor1_num));
        assert!(batch_map.contains_key(&anchor2_num));
        assert_eq!(batch_map[&anchor1_num].len(), 1); // One batch for anchor 10
        assert_eq!(batch_map[&anchor1_num][0].blocks().len(), 2); // Two blocks in that batch
        assert_eq!(batch_map[&anchor2_num].len(), 1); // One batch for anchor 11
        assert_eq!(batch_map[&anchor2_num][0].blocks().len(), 1); // One block in that batch
        assert_eq!(batch_map.count(), 2);
    }

    #[tokio::test]
    async fn test_new_batch_map_batch_splitting() {
        let ts = current_timestamp();
        let anchor1_num = 10;
        let mut settings = *TEST_SETTINGS;
        settings.capacity = 2; // Force splitting using the correct field name

        let blocks = vec![
            mock_anchored_block(100, ts, BENEFICIARY, anchor1_num, ts - 10),
            mock_anchored_block(101, ts + 1, BENEFICIARY, anchor1_num, ts - 10),
            mock_anchored_block(102, ts + 2, BENEFICIARY, anchor1_num, ts - 10),
        ];

        let batch_map = BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), settings);

        assert_eq!(batch_map.len(), 1);
        assert!(batch_map.contains_key(&anchor1_num));
        assert_eq!(batch_map[&anchor1_num].len(), 2); // Should split into two batches
        assert_eq!(batch_map[&anchor1_num][0].blocks().len(), 2);
        assert_eq!(batch_map[&anchor1_num][1].blocks().len(), 1);
        assert_eq!(batch_map.count(), 2);
    }

    #[tokio::test]
    async fn test_retimestamp() {
        let ts = current_timestamp();
        let anchor1_num = 10;
        let blocks = vec![
            mock_anchored_block(100, ts - 100, BENEFICIARY, anchor1_num, ts - 110), /* Too early */
            mock_anchored_block(101, ts, BENEFICIARY, anchor1_num, ts - 110),       // OK
            mock_anchored_block(102, ts + 100, BENEFICIARY, anchor1_num, ts - 110), /* Too late */
        ];

        let min_ts = ts - 50;
        let max_ts = ts + 50;
        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), *TEST_SETTINGS)
                .ensure_unique_coinbase()
                .reanchor(anchor1_num, &mock_header(anchor1_num, ts), *TEST_SETTINGS)
                .retimestamp(min_ts, max_ts);

        let batch = &batch_map[&anchor1_num][0];
        assert_eq!(batch.blocks()[0].timestamp(), max_ts); // reset to max timestamp
        assert_eq!(batch.blocks()[1].timestamp(), max_ts); // reset to max timestamp
        assert_eq!(batch.blocks()[2].timestamp(), max_ts); // reset to max timestamp
    }

    #[test]
    fn test_should_reanchor() {
        let ts = current_timestamp();
        let anchor1_num = 10;
        let anchor2_num = 20;
        let blocks = vec![
            mock_anchored_block(100, ts, BENEFICIARY, anchor1_num, ts - 10),
            mock_anchored_block(101, ts + 1, BENEFICIARY, anchor2_num, ts),
        ];

        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), *TEST_SETTINGS);

        assert!(batch_map.should_reanchor(15)); // min anchor 10 < 15
        assert!(!batch_map.should_reanchor(10)); // min anchor 10 == 10
        assert!(!batch_map.should_reanchor(5)); // min anchor 10 > 5

        let empty_map = BatchMapInitialized::default();
        assert!(!empty_map.should_reanchor(10));
    }

    #[tokio::test]
    async fn test_reanchor() {
        let ts = current_timestamp();
        let old_anchor_num = 10;
        let new_anchor_num = 15;
        let blocks = vec![
            mock_anchored_block(100, ts, BENEFICIARY, old_anchor_num, ts - 10),
            mock_anchored_block(101, ts + 1, BENEFICIARY, old_anchor_num, ts - 10),
        ];

        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), *TEST_SETTINGS);
        assert!(batch_map.contains_key(&old_anchor_num));
        assert!(!batch_map.contains_key(&new_anchor_num));
        assert_eq!(batch_map[&old_anchor_num][0].blocks().len(), 2);

        let safe_anchor = mock_header(new_anchor_num, ts + 5);
        let batch_map = batch_map.ensure_unique_coinbase().reanchor(
            safe_anchor.number,
            &safe_anchor,
            *TEST_SETTINGS,
        );

        assert!(!batch_map.contains_key(&old_anchor_num)); // Old anchor gone
        assert!(batch_map.contains_key(&new_anchor_num)); // New anchor present
        assert_eq!(batch_map.len(), 1);
        assert_eq!(batch_map.count(), 1);
        assert_eq!(batch_map[&new_anchor_num].len(), 1);
        let reanchored_batch = &batch_map[&new_anchor_num][0];
        assert_eq!(reanchored_batch.anchor_id(), new_anchor_num);
        assert_eq!(reanchored_batch.anchor().hash, safe_anchor.hash);
        assert_eq!(reanchored_batch.blocks().len(), 2); // Both blocks re-anchored
        assert_eq!(reanchored_batch.blocks()[0].number(), 100);
        assert_eq!(reanchored_batch.blocks()[1].number(), 101);
    }

    #[tokio::test]
    async fn test_use_original_coinbase() {
        let ts = current_timestamp();
        let anchor_num = 10;
        let coinbase1 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let coinbase2 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let blocks = vec![
            mock_anchored_block(100, ts, coinbase1, anchor_num, ts - 10),
            mock_anchored_block(101, ts + 1, coinbase1, anchor_num, ts - 10),
            mock_anchored_block(102, ts + 2, coinbase2, anchor_num, ts - 10), // Different coinbase
            mock_anchored_block(103, ts + 3, coinbase1, anchor_num, ts - 10), // Back to first
            mock_anchored_block(104, ts + 4, coinbase2, anchor_num, ts - 10), // Back to second
        ];

        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), *TEST_SETTINGS);
        assert_eq!(batch_map.count(), 1); // Starts as one batch

        let batch_map = batch_map.ensure_unique_coinbase();

        assert_eq!(batch_map.len(), 1); // Still one anchor key
        assert!(batch_map.contains_key(&anchor_num));

        let batches = &batch_map[&anchor_num];
        assert_eq!(batches.len(), 4); // Should be split into 4 batches

        // Check batches
        assert_eq!(batches[0].blocks().len(), 2);
        assert_eq!(batches[0].blocks()[0].coinbase(), coinbase1);
        assert_eq!(batches[0].blocks()[1].coinbase(), coinbase1);

        assert_eq!(batches[1].blocks().len(), 1);
        assert_eq!(batches[1].blocks()[0].coinbase(), coinbase2);

        assert_eq!(batches[2].blocks().len(), 1);
        assert_eq!(batches[2].blocks()[0].coinbase(), coinbase1);

        assert_eq!(batches[3].blocks().len(), 1);
        assert_eq!(batches[3].blocks()[0].coinbase(), coinbase2);

        // Check total count
        assert_eq!(batch_map.count(), 4);
    }

    #[tokio::test]
    async fn test_use_original_coinbase_no_split() {
        let ts = current_timestamp();
        let anchor_num = 10;
        let coinbase1 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        let blocks = vec![
            mock_anchored_block(100, ts, coinbase1, anchor_num, ts - 10),
            mock_anchored_block(101, ts + 1, coinbase1, anchor_num, ts - 10),
        ];

        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks).unwrap(), *TEST_SETTINGS);
        assert_eq!(batch_map.count(), 1);

        let batch_map = batch_map.ensure_unique_coinbase();

        assert_eq!(batch_map.count(), 1); // No split needed
        let batches = &batch_map[&anchor_num];
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].blocks().len(), 2);
        assert_eq!(batches[0].blocks()[0].coinbase(), coinbase1);
        assert_eq!(batches[0].blocks()[1].coinbase(), coinbase1);
    }

    #[tokio::test]
    async fn test_into_vec() {
        let ts = current_timestamp();
        let anchor1_num = 10;
        let anchor2_num = 11;
        let blocks = vec![
            mock_anchored_block(100, ts, BENEFICIARY, anchor1_num, ts - 10),
            mock_anchored_block(101, ts + 1, BENEFICIARY, anchor2_num, ts),
            mock_anchored_block(102, ts + 2, BENEFICIARY, anchor2_num, ts),
            mock_anchored_block(103, ts + 3, BENEFICIARY, anchor2_num, ts),
        ];

        let mut settings = *TEST_SETTINGS;
        settings.capacity = 2; // Force split on anchor 2

        let batch_map =
            BatchMap::new(ContiguousAnchoredBlocks::new(blocks.clone()).unwrap(), settings)
                .ensure_unique_coinbase()
                .reanchor(anchor1_num, blocks[3].anchor(), settings)
                .retimestamp(ts - 10, ts + 10);
        assert_eq!(batch_map.count(), 3); // 3 batches in total

        let batch_vec = batch_map.into_vec();
        assert_eq!(batch_vec.len(), 3); // 3 batches in total

        // Check order (should be ordered by anchor block number, then batch creation order)

        assert_eq!(batch_vec[0].anchor_id(), anchor1_num);
        assert_eq!(batch_vec[0].blocks()[0].number(), 100);

        assert_eq!(batch_vec[1].anchor_id(), anchor2_num);
        assert_eq!(batch_vec[1].blocks()[0].number(), 101);
        assert_eq!(batch_vec[1].blocks()[1].number(), 102);

        assert_eq!(batch_vec[2].anchor_id(), anchor2_num);
        assert_eq!(batch_vec[2].blocks()[0].number(), 103);
    }

    #[tokio::test] // needs tokio runtime
    async fn test_pop_outstanding_different_anchors() {
        let proposer = Address::ZERO;
        let other_proposer = Address::new([1; 20]);
        let blocks = vec![
            mock_anchored_block(100, current_timestamp(), other_proposer, 9, current_timestamp()), /* Unsafe block, ignored */
            mock_anchored_block(101, current_timestamp(), proposer, 9, current_timestamp()), /* low anchor */
            mock_anchored_block(102, current_timestamp(), proposer, 10, current_timestamp()), /* highest anchor */
            mock_anchored_block(103, current_timestamp(), proposer, 10, current_timestamp()), /* highest anchor */
        ];
        let blocks = ContiguousAnchoredBlocks::new(blocks).unwrap();

        let batch_map = BatchMap::new(blocks.clone(), *TEST_SETTINGS)
            .ensure_unique_coinbase()
            .reanchor(9, blocks[3].anchor(), *TEST_SETTINGS)
            .retimestamp(0, Timestamp::MAX);

        let batch = batch_map.into_vec().pop_with_coinbase(proposer).unwrap();

        assert_eq!(batch.anchor().number, 10);
        assert_eq!(batch.blocks().len(), 2);
        assert_eq!(batch.blocks()[0].number(), 102);
        assert_eq!(batch.blocks()[1].number(), 103);
    }

    #[tokio::test] // needs tokio runtime
    async fn test_pop_outstanding_same_anchor_more_than_capacity() {
        let proposer = Address::ZERO;
        let blocks = vec![
            mock_anchored_block(100, current_timestamp(), proposer, 10, current_timestamp()), /* Unsafe block, ignored */
            mock_anchored_block(101, current_timestamp(), proposer, 10, current_timestamp()), /* low anchor */
            mock_anchored_block(102, current_timestamp(), proposer, 10, current_timestamp()), /* highest anchor */
            mock_anchored_block(103, current_timestamp(), proposer, 10, current_timestamp()), /* highest anchor */
        ];
        let blocks = ContiguousAnchoredBlocks::new(blocks).unwrap();

        let mut settings = *TEST_SETTINGS;
        settings.capacity = 2;

        let batch_map = BatchMap::new(blocks.clone(), settings)
            .ensure_unique_coinbase()
            .reanchor(9, blocks[3].anchor(), settings)
            .retimestamp(0, Timestamp::MAX);

        let batch = batch_map.into_vec().pop_with_coinbase(proposer).unwrap();

        assert_eq!(batch.anchor().number, 10);
        assert_eq!(batch.blocks().len(), 2);
        assert_eq!(batch.blocks()[0].number(), 102);
        assert_eq!(batch.blocks()[1].number(), 103);
    }
}
