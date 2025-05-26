use std::{self, ops::RangeInclusive, sync::Arc};

use alloy::{
    consensus::TxEnvelope,
    eips::merge::SLOT_DURATION_SECS,
    rpc::types::{Block, Header},
};
use alloy_primitives::{Address, BlockNumber};
use derive_more::derive::{Deref, Into, IntoIterator, IsVariant};
use tokio::sync::oneshot;
use tracing::debug;

use crate::{
    compression::{ZlibCompressionEstimate, rlp_encode_and_compress_in_background},
    summary::Summary,
    time::Timestamp,
};

/// An enum indicating whether a batch will reorg or not when posted.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WillReorg {
    Yes,
    No,
}

use super::constants::{TAIKO_GOLDEN_TOUCH_ACCOUNT, TIMESHIFT_BUFFER_SECS};

/// The settings of a [`Batch`].
#[derive(Debug, Clone, Copy)]
pub struct BatchSettings {
    /// The maximum number of blocks that can be in the batch.
    pub capacity: usize,
    /// The target compressed size of the batch, in bytes.
    pub target_compressed_size: usize,
    /// The maximum raw size of the batch, in bytes (after compression).
    pub max_raw_size: usize,
    /// The default coinbase address of all blocks in the batch.
    pub default_coinbase: Address,
    /// The maximum age of the anchor block, measured from the L1 chain tip.
    pub max_anchor_height_offset: u64,
    /// The buffer to account for the maximum anchor block height used in a batch transaction.
    /// This is used to ensure that the batch will be reset if the anchor block is getting close
    /// to its age limit. If the limit expires, the batch will revert with
    /// `AnchorBlockIdTooSmall()` error.
    pub anchor_max_height_buffer: u64,
}

/// A batch of blocks that have been produced but not yet proposed to the L1.
/// When a batch can be expanded with new blocks, it is also said to be "outstanding".
///
/// - every [`PreconfirmedBlock`] in this batch MUST have the same anchor block.
/// - the batch MUST not contain more than [`BatchSettings::capacity`] blocks.
#[derive(Debug, Clone)]
pub struct Batch {
    /// The list of blocks in the batch.
    blocks: Vec<PreconfirmedBlock>,
    /// The shared anchor block of all anchor transactions in the batch.
    anchor_block: Header,
    /// The settings of the batch.
    settings: BatchSettings,
    /// The coinbase address of all blocks in the batch.
    coinbase: Address,
}

/// The reason why a block could not be added to a [`Batch`].
#[derive(Debug, Clone, Copy)]
#[allow(missing_docs)]
pub enum DoesntFitBlockReason {
    Capacity,
    TargetSize { actual: usize, target: usize },
    AnchorChanged { old: BlockNumber, new: BlockNumber },
}

impl Batch {
    /// Creates a new empty batch with the given anchor block and capacity.
    /// All invariants are valid because the batch is still empty.
    pub const fn new(anchor_block: Header, settings: BatchSettings) -> Self {
        Self { settings, anchor_block, blocks: Vec::new(), coinbase: settings.default_coinbase }
    }

    /// Creates a new batch with the given anchor block and blocks.
    ///
    /// This function does not check if the anchor block is consistent OR if the
    /// blocks exceed the capacity. It should be used with caution.
    pub const fn from_blocks_unchecked(
        anchor_block: Header,
        settings: BatchSettings,
        blocks: Vec<PreconfirmedBlock>,
    ) -> Self {
        Self { settings, blocks, anchor_block, coinbase: settings.default_coinbase }
    }

    /// Returns whether batch can fit a new a block with the provided `tx_list`.
    /// If that's not the case, `Some(DoesntFitBlockReason)` is returned.
    pub fn doesnt_fit(&self, tx_list: &[TxEnvelope]) -> Option<DoesntFitBlockReason> {
        let new_block_size: usize = tx_list.iter().map(|tx| tx.eip2718_encoded_length()).sum();
        let new_raw_size = self.raw_size() + new_block_size;

        // Calculate the target size for the batch, based on the weighted average of the
        // ZLIB compression ratios of the historical compression samples.
        let new_compressed_size = ZlibCompressionEstimate::get(new_raw_size);

        // If the new size is above the target, we should split the batch into two, return
        // the first one with the existing blocks, and add the new block to the second one.
        if new_compressed_size > self.settings.target_compressed_size {
            return Some(DoesntFitBlockReason::TargetSize {
                actual: new_compressed_size,
                target: self.settings.target_compressed_size,
            });
        }

        // If the batch is full of blocks, reset it with the same anchor block
        if self.blocks.len() == self.settings.capacity {
            return Some(DoesntFitBlockReason::Capacity);
        }

        None
    }

    /// Adds a block to the batch. If the batch is full, it is taken and it will be returned.
    /// Then, `self` is reset with the `new_anchor` provided.
    ///
    /// INVARIANT: after calling this function, `self.blocks.len() > 0`.
    pub fn add_block_and_reset_if_full(
        &mut self,
        block: PreconfirmedBlock,
        new_anchor: &Header,
    ) -> AddBlockResult {
        // If we're adding a block which number is already known, it means
        // taiko-geth has deleted the next ones and as such we should drop them from our batch.
        self.retain_blocks_before(block.number);

        let Some(reason) = self.doesnt_fit(&block.tx_list) else {
            // The batch is neither full nor over the target size, so we can add the block to it.
            self.blocks.push(block);
            return AddBlockResult::Added { raw_size: self.raw_size() };
        };

        // We create a new batch and push the new block in it.
        let this = self.take_and_reset(new_anchor.clone());
        self.blocks.push(block);

        AddBlockResult::Full { reason, batch: this }
    }

    /// Returns `true` if the last block in the batch is close to the maximum timeshift buffer, in
    /// which case we should build an empty block with `new_block_ts` to reset the timeshift.
    pub fn should_build_empty_block(&self, new_block_ts: Timestamp) -> bool {
        let Some(last_ts) = self.last_timestamp() else {
            return false;
        };

        let max = new_block_ts.saturating_sub(TIMESHIFT_BUFFER_SECS);

        last_ts <= max
    }

    /// Returns `true` if the batch should be re-anchored, based on the anchor block age and the
    /// next block timestamp. See [`Self::compute_age_limit`] for more details.
    pub fn should_reanchor(
        &self,
        next_block_ts: Timestamp,
        next_block_number: BlockNumber,
    ) -> bool {
        let slots_left = self.compute_age_limit(next_block_ts, next_block_number);

        slots_left < self.settings.anchor_max_height_buffer
    }

    /// This function computes the age limit expressed in slots left before the batch is
    /// considered stale and needs to be re-anchored. It takes into account both the age of the
    /// anchor block and the age of the first timestamp in the batch.
    ///
    /// # Params:
    /// - `next_block_ts`: The timestamp of the next L1 block.
    /// - `next_block_number`: The number of the next L1 block.
    pub fn compute_age_limit(
        &self,
        next_block_ts: Timestamp,
        next_block_number: BlockNumber,
    ) -> u64 {
        let anchor_age = next_block_number.saturating_sub(self.anchor_id());
        let anchor_slots_left = self.settings.max_anchor_height_offset.saturating_sub(anchor_age);

        let timeshit_limit = self.settings.max_anchor_height_offset * SLOT_DURATION_SECS;

        if let Some(first_timestamp) = self.first_timestamp() {
            // Timeshift slots left: get the diff between the first timestamp and the next block
            // timestamp, subtract it from the timeshift limit and divide by the slot
            // duration to get the number of slots left before we reach it.
            let distance = next_block_ts.saturating_sub(first_timestamp);
            let timeshift_seconds_left = timeshit_limit.saturating_sub(distance);
            let timeshift_slots_left = timeshift_seconds_left / SLOT_DURATION_SECS;

            return anchor_slots_left.min(timeshift_slots_left);
        }

        anchor_slots_left
    }

    /// Retains all blocks from the batch whose number is greater than the given block number.
    pub fn retain_blocks_after(&mut self, block_number: u64) {
        let before = self.blocks.len();
        self.blocks.retain(|block| block.number > block_number);
        let after = self.blocks.len();
        if before > after {
            debug!(before, after, "Dropped {} blocks from batch <= {block_number}", before - after);
            debug!(summary = %self.summary(), "Outstanding batch");
        }
    }

    /// Retains all blocks from the batch whose number is less than the given block number.
    pub fn retain_blocks_before(&mut self, block_number: u64) {
        let before = self.blocks.len();
        self.blocks.retain(|block| block.number < block_number);
        let after = self.blocks.len();
        if before > after {
            debug!(before, after, "Dropped {} blocks from batch >= {block_number}", before - after);
            debug!(summary = %self.summary(), "Outstanding batch");
        }
    }

    /// Returns the total number of transactions in the batch,
    /// as the sum of the length of every `tx_list` in each block.
    pub fn tx_count(&self) -> usize {
        self.blocks.iter().map(|block| block.tx_list.len()).sum()
    }

    /// Returns a reference to the anchor block of the batch.
    pub const fn anchor(&self) -> &Header {
        &self.anchor_block
    }

    /// Returns the anchor block number.
    pub const fn anchor_id(&self) -> u64 {
        self.anchor_block.inner.number
    }

    /// Returns a reference to the blocks in the batch.
    ///
    /// `const_vec_string_slice` will be available in Rust 1.87
    #[allow(clippy::missing_const_for_fn)]
    pub fn blocks(&self) -> &[PreconfirmedBlock] {
        &self.blocks
    }

    /// Returns the range of block numbers in the batch.
    ///
    /// If empty, returns `0..=0`.
    pub fn blocks_range(&self) -> RangeInclusive<u64> {
        if self.blocks.is_empty() {
            return 0..=0;
        }

        self.blocks.first().expect("not empty").number..=
            self.blocks.last().expect("not empty").number
    }

    /// Returns the blocks in the batch as a vector, consuming the batch.
    pub fn into_blocks(mut self) -> Vec<PreconfirmedBlock> {
        std::mem::take(&mut self.blocks)
    }

    /// Returns the timestamp of the first and last block in the batch.
    /// Returns `None` if the batch is empty.
    pub fn first_and_last_block_timestamps(&self) -> Option<(u64, u64)> {
        if self.blocks.is_empty() {
            return None;
        }

        Some((
            self.blocks.first().expect("not empty").timestamp,
            self.blocks.last().expect("not empty").timestamp,
        ))
    }

    /// Returns the timestamp of the first block in the batch.
    pub fn first_timestamp(&self) -> Option<u64> {
        self.blocks.first().map(|b| b.timestamp)
    }

    /// Returns the timestamp of the last block in the batch.
    pub fn last_timestamp(&self) -> Option<u64> {
        self.blocks.last().map(|b| b.timestamp)
    }

    /// Returns the total time shift of the batch.
    pub fn total_time_shift(&self) -> u64 {
        if self.blocks.is_empty() {
            return 0;
        }

        self.blocks.last().expect("not empty").timestamp -
            self.blocks.first().expect("not empty").timestamp
    }

    /// Returns the current batch, and then resets the anchor block to the provided one.
    pub fn take_and_reset(&mut self, new_anchor_block: Header) -> Self {
        let this = Self {
            settings: self.settings,
            blocks: self.blocks.drain(..).collect(),
            anchor_block: self.anchor_block.clone(),
            coinbase: self.coinbase,
        };

        // Reset the remaining contents. Blocks are drained above.
        self.anchor_block = new_anchor_block;

        this
    }

    /// Changes the anchor block of the entire batch.
    pub fn change_anchor(&mut self, new_anchor: Header) {
        self.anchor_block = new_anchor;
    }

    /// Changes the coinbase address of all blocks in the batch.
    pub const fn change_coinbase(&mut self, new_coinbase: Address) {
        self.coinbase = new_coinbase;
    }

    /// Returns the coinbase address of all blocks in the batch.
    pub const fn coinbase(&self) -> Address {
        self.coinbase
    }

    /// Sets the timestamp of all blocks in the batch to the given timestamp.
    pub fn set_all_blocks_timestamp(&mut self, timestamp: Timestamp) {
        for block in &mut self.blocks {
            block.timestamp = timestamp;
        }
    }

    /// Splits the batch into multiple batches, based on the coinbase address of the blocks.
    ///
    /// Example:
    /// - Batch with blocks 3, 4, 5, 6.
    /// - Blocks 3 and 4 have coinbase A, and blocks 5 and 6 have coinbase B.
    /// - This function will return two batches: [3, 4] and [5, 6].
    pub fn split_by_coinbase(self) -> Vec<Self> {
        let Some(first_block) = self.blocks.first() else {
            return vec![];
        };

        let mut batches = Vec::new();
        let mut current_batch = Self::new(self.anchor_block.clone(), self.settings);
        current_batch.change_coinbase(first_block.coinbase());

        for block in self.blocks {
            if current_batch.coinbase() != block.coinbase() {
                debug!(
                    start_block = block.number,
                    old_coinbase = %current_batch.coinbase(),
                    new_coinbase = %block.coinbase(),
                    "Splitting batch by coinbase"
                );

                batches.push(current_batch);
                current_batch = Self::new(self.anchor_block.clone(), self.settings);
                current_batch.change_coinbase(block.coinbase());
            }

            current_batch.blocks.push(block);
        }

        batches.push(current_batch);
        batches
    }

    /// Returns `true` if the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Returns the estimated raw size of the batch, in bytes.
    /// This is used as an initial approximation for the size target of the batch.
    pub fn raw_size(&self) -> usize {
        self.blocks().iter().map(|b| b.raw_size()).sum()
    }

    /// Triggers the compression of the batch with Zlib in a background thread, yielding
    /// the result in a oneshot channel.
    pub fn trigger_compression(&self) -> oneshot::Receiver<usize> {
        let raw_data = self.blocks.iter().map(|b| Arc::clone(&b.tx_list)).collect::<Vec<_>>();
        rlp_encode_and_compress_in_background(raw_data)
    }
}

/// [`OrderedBatches`] are batches with an increasing anchor block number,
/// with a change in coinbase in the last position, if any.
///
/// # Example:
///
/// The following batches are ordered:
/// - 1. batch with coinbase A, anchor block 1, blocks 10..20
/// - 2. batch with coinbase A, anchor block 2, blocks 21..30
/// - 3. batch with coinbase A, anchor block 2, blocks 31..40
/// - 4. batch with coinbase B, anchor block 2, blocks 41..50
/// - 5. batch with coinbase B, anchor block 3, blocks 51..60
#[derive(Debug, Clone, Default, Deref, IntoIterator, Into)]
pub struct OrderedBatches(Vec<Batch>);

impl OrderedBatches {
    /// Creates a new instance of [`Self`] from a vector of batches.
    pub const fn new_unchecked(batches: Vec<Batch>) -> Self {
        Self(batches)
    }

    /// Returns the last batch ONLY IF it matches the given coinbase address.
    ///
    /// NOTE: since when we're in the sequencing window only we can propose blocks,
    /// if `coinbase` matches the sequencer's address, then this returns the outstanding batch.
    pub fn pop_with_coinbase(&mut self, coinbase: Address) -> Option<Batch> {
        let last = self.0.last()?;

        if last.coinbase != coinbase {
            return None;
        }

        self.0.pop()
    }
}

impl Summary for Batch {
    fn summary(&self) -> String {
        format!(
            "(range={:?}, blocks={}, txs={}, raw={}b, anchor={})",
            self.blocks_range(),
            self.blocks().len(),
            self.tx_count(),
            self.raw_size(),
            self.anchor_id()
        )
    }
}

/// The possible results of adding a block to a [`Batch`] using
/// [`Batch::add_block_and_reset_if_full`]
#[derive(Debug, IsVariant)]
#[allow(missing_docs)]
pub enum AddBlockResult {
    /// The block was added to the batch successfully
    Added { raw_size: usize },
    /// The block couldn't fit in the batch. The reason for it is returned
    /// along with the batch. The block has been added to a new batch.
    Full { reason: DoesntFitBlockReason, batch: Batch },
}

/// The format of a Taiko L2 block that is preconfirmed and waiting to be proposed to the L1.
///
/// This block MUST NOT contain an anchor transaction, as it is filled by `taiko-client`.
/// Reference: <https://github.com/taikoxyz/taiko-mono/blob/ce5bcca7d2f94105c9dabdf2e644a29f1baf7b4d/packages/taiko-client/driver/chain_syncer/blob/blocks_inserter/pacaya.go#L177-L186>
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PreconfirmedBlock {
    /// The block timestamp.
    timestamp: Timestamp,
    /// The block number.
    number: BlockNumber,
    /// The coinbase address of the block.
    coinbase: Address,
    /// The list of transactions in the block.
    ///
    /// This MUST NOT contain the anchor transaction.
    tx_list: Arc<Vec<TxEnvelope>>,
}

impl PreconfirmedBlock {
    /// Creates an instance of [`Self`] from a header and a list of transactions
    pub fn new(header: &Header, tx_list: Vec<TxEnvelope>) -> Self {
        Self {
            timestamp: header.timestamp,
            number: header.number,
            tx_list: Arc::new(tx_list),
            coinbase: header.beneficiary,
        }
    }

    /// Returns a reference to the timestamp of the block.
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns a reference to the number of the block.
    pub const fn number(&self) -> BlockNumber {
        self.number
    }

    /// Returns the coinbase address of the block.
    pub const fn coinbase(&self) -> Address {
        self.coinbase
    }

    /// Returns a reference to the list of transactions in the block, without the anchor tx.
    ///
    /// `const_vec_string_slice` will be available in Rust 1.87
    #[allow(clippy::missing_const_for_fn)]
    pub fn tx_list(&self) -> &[TxEnvelope] {
        &self.tx_list
    }

    /// Returns the raw encoded size of the transactions in the block, in bytes.
    pub fn raw_size(&self) -> usize {
        self.tx_list.iter().map(|tx| tx.eip2718_encoded_length()).sum()
    }

    /// Creates an instance of [`Self`] from a full block, by filtering out anchor transactions
    /// and converting them to EIP-2718 bytes.
    pub fn from_block(value: Block) -> Self {
        let timestamp = value.header.timestamp;
        let number = value.header.number;
        let tx_list = value
            .transactions
            .into_transactions()
            // Filter out anchor transactions, done by the taiko system address
            .filter(|tx| tx.inner.signer() != TAIKO_GOLDEN_TOUCH_ACCOUNT)
            .map(|tx| tx.into_inner())
            .collect::<Vec<_>>();

        Self { number, timestamp, tx_list: Arc::new(tx_list), coinbase: value.header.beneficiary }
    }
}

impl Summary for PreconfirmedBlock {
    fn summary(&self) -> String {
        format!(
            "(num={}, ts={}, txs={}, coinbase={})",
            self.number,
            self.timestamp,
            self.tx_list.len(),
            self.coinbase
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy::eips::Decodable2718;

    use super::*;

    impl Default for BatchSettings {
        fn default() -> Self {
            Self {
                capacity: 500,
                target_compressed_size: 500_000,
                max_raw_size: 500_000,
                default_coinbase: TAIKO_GOLDEN_TOUCH_ACCOUNT,
                max_anchor_height_offset: 64,
                anchor_max_height_buffer: 6,
            }
        }
    }

    fn create_test_header(timestamp: u64, number: u64) -> Header {
        Header {
            inner: alloy::consensus::Header { number, timestamp, ..Default::default() },
            ..Default::default()
        }
    }

    fn create_test_block(timestamp: u64, number: u64) -> PreconfirmedBlock {
        let b = alloy::hex!(
            "02f87783028c6282fe9f843b9aca00843b9aca008252089488f98e33ad59e61deb864f33286acdf6073f27d3880de0b6b3a764000080c001a0db71009555f988fd8eb6cafef2195738c99a97480ec3294f11e779fa3c40188fa03a4e7215f37409d8498460011732667e32b373921c826184accd98a93d75616d"
        );
        let tx_list = vec![TxEnvelope::decode_2718(&mut b.as_ref()).unwrap()];
        PreconfirmedBlock {
            timestamp,
            number,
            tx_list: Arc::new(tx_list),
            coinbase: TAIKO_GOLDEN_TOUCH_ACCOUNT,
        }
    }

    #[test]
    fn test_batch_creation() {
        let anchor = create_test_header(1000, 1);
        let batch = Batch::new(anchor.clone(), BatchSettings::default());
        assert_eq!(batch.settings.capacity, 500);
        assert_eq!(batch.settings.target_compressed_size, 500_000);
        assert!(batch.blocks.is_empty());
        assert_eq!(batch.anchor_block, anchor);
    }

    // Note: tokio runtime required for `add_block_and_reset_if_full`
    #[tokio::test]
    async fn test_add_block_and_reset_if_full() {
        let anchor = create_test_header(1000, 1);
        let settings = BatchSettings {
            capacity: 2,
            target_compressed_size: 1000,
            max_raw_size: 1000,
            default_coinbase: TAIKO_GOLDEN_TOUCH_ACCOUNT,
            max_anchor_height_offset: 64,
            anchor_max_height_buffer: 6,
        };
        let mut batch = Batch::new(anchor.clone(), settings);

        let block1 = create_test_block(1001, 2);
        let block2 = create_test_block(1002, 3);
        let block3 = create_test_block(1003, 4);

        // First block should be added without reset
        assert!(batch.add_block_and_reset_if_full(block1.clone(), &anchor).is_added());

        // Second block should be added without reset
        assert!(batch.add_block_and_reset_if_full(block2.clone(), &anchor).is_added());

        // Second block should trigger reset because the batch is full
        let AddBlockResult::Full { batch: full_batch, .. } =
            batch.add_block_and_reset_if_full(block3.clone(), &anchor)
        else {
            panic!("Expected reset result");
        };

        assert_eq!(full_batch.blocks.len(), 2);
        assert_eq!(full_batch.blocks[0], block1);
        assert_eq!(full_batch.blocks[1], block2);

        // Check that the batch contains block3
        assert_eq!(batch.blocks.len(), 1);
        assert_eq!(batch.blocks.pop().unwrap(), block3);
        assert_eq!(batch.anchor_block, anchor);
    }

    #[test]
    fn test_blocks_range() {
        let anchor = create_test_header(1000, 1);
        let mut batch = Batch::new(anchor, BatchSettings::default());

        let block1 = create_test_block(1001, 2);
        let block2 = create_test_block(1002, 3);
        let block3 = create_test_block(1003, 4);

        // Add blocks one by one without resetting
        batch.blocks.push(block1);
        batch.blocks.push(block2);
        batch.blocks.push(block3);

        let range = batch.blocks_range();
        assert_eq!(range.start(), &2);
        assert_eq!(range.end(), &4);
    }

    #[test]
    fn test_preconfirmed_block_creation() {
        let header = create_test_header(1000, 1);
        let b = alloy::hex!(
            "02f87783028c6282fe9f843b9aca00843b9aca008252089488f98e33ad59e61deb864f33286acdf6073f27d3880de0b6b3a764000080c001a0db71009555f988fd8eb6cafef2195738c99a97480ec3294f11e779fa3c40188fa03a4e7215f37409d8498460011732667e32b373921c826184accd98a93d75616d"
        );
        let tx_list = vec![TxEnvelope::decode_2718(&mut b.as_ref()).unwrap()];

        let block = PreconfirmedBlock::new(&header, tx_list.clone());

        assert_eq!(block.timestamp(), 1000);
        assert_eq!(block.number(), 1);
        assert_eq!(block.tx_list(), tx_list.as_slice());
    }

    #[test]
    fn test_preconfirmed_block_from_block() {
        let header = create_test_header(1000, 1);
        let block = Block { header, ..Default::default() };

        let preconfirmed = PreconfirmedBlock::from_block(block);

        assert_eq!(preconfirmed.timestamp(), 1000);
        assert_eq!(preconfirmed.number(), 1);
        assert!(preconfirmed.tx_list().is_empty());
    }

    #[test]
    fn test_compute_age_limit_empty_batch() {
        let anchor = create_test_header(1000, 1);
        let batch = Batch::new(anchor, BatchSettings::default());

        // next block is a few slots ahead
        let next_block_ts: u64 = 1000 + SLOT_DURATION_SECS * 3;
        let next_block_num: u64 = 4;

        let anchor_age = next_block_num.saturating_sub(1);
        let expected_anchor_slots_left =
            BatchSettings::default().max_anchor_height_offset.saturating_sub(anchor_age);

        assert_eq!(
            batch.compute_age_limit(next_block_ts, next_block_num),
            expected_anchor_slots_left
        );
    }

    #[test]
    fn test_compute_age_limit_with_blocks() {
        let anchor = create_test_header(1000, 1);
        let settings = BatchSettings::default();
        let mut batch = Batch::new(anchor.clone(), settings);

        // add a block so first_timestamp is set
        batch.blocks.push(create_test_block(1010, 2));

        let next_block_ts: u64 = 1050;
        let next_block_num: u64 = 8;

        let anchor_age = next_block_num.saturating_sub(anchor.inner.number);
        let anchor_slots_left = settings.max_anchor_height_offset.saturating_sub(anchor_age);

        let timeshift_limit = settings.max_anchor_height_offset * SLOT_DURATION_SECS;
        let distance = next_block_ts.saturating_sub(1010u64);
        let timeshift_seconds_left = timeshift_limit.saturating_sub(distance);
        let timeshift_slots_left = timeshift_seconds_left / SLOT_DURATION_SECS;
        let expected = anchor_slots_left.min(timeshift_slots_left);

        assert_eq!(batch.compute_age_limit(next_block_ts, next_block_num), expected);
    }

    #[test]
    fn test_should_build_empty_block() {
        let anchor = create_test_header(1000, 1);
        let mut batch = Batch::new(anchor, BatchSettings::default());

        // empty batch should return false
        assert!(!batch.should_build_empty_block(1100));

        // add a block well in the past
        batch.blocks.push(create_test_block(1000, 2));
        let ts = 1000 + TIMESHIFT_BUFFER_SECS + 1;
        assert!(batch.should_build_empty_block(ts));

        // newer block should not trigger empty block
        batch.blocks.clear();
        batch.blocks.push(create_test_block(ts, 3));
        assert!(!batch.should_build_empty_block(ts + 1));
    }
}
