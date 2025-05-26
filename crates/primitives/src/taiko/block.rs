use std::collections::HashMap;

use alloy::rpc::types::{Block, Header};
use alloy_primitives::BlockNumber;
use derive_more::derive::{Deref, Into, IntoIterator};

use super::batch::{Batch, BatchSettings, PreconfirmedBlock};

/// An L2 block that has been anchored to an L1 block
#[derive(Debug, Clone)]
pub struct AnchoredBlock {
    /// The L2 block
    block: Block,
    /// The L1 anchor header for this block
    anchor: Header,
}

impl AnchoredBlock {
    /// Create a new [`AnchoredBlock`] from a [`Block`] and a [`Header`]
    pub const fn new(block: Block, anchor: Header) -> Self {
        Self { block, anchor }
    }

    /// Consume this [`AnchoredBlock`] and return its components
    pub fn into_parts(self) -> (Block, Header) {
        (self.block, self.anchor)
    }

    /// Returns a reference to the block
    pub const fn block(&self) -> &Block {
        &self.block
    }

    /// Consume this [`AnchoredBlock`] and return the [`Block`]
    pub fn into_block(self) -> Block {
        self.block
    }

    /// Consume this [`AnchoredBlock`] and return a [`PreconfirmedBlock`]
    pub fn into_preconfirmed(self) -> PreconfirmedBlock {
        PreconfirmedBlock::from_block(self.block)
    }

    /// Returns a reference to the anchor header
    pub const fn anchor(&self) -> &Header {
        &self.anchor
    }

    /// Consume this [`AnchoredBlock`] and return the anchor header
    pub fn into_anchor(self) -> Header {
        self.anchor
    }

    /// Returns the block number of the anchor block
    #[allow(clippy::missing_const_for_fn)]
    pub fn anchor_id(&self) -> BlockNumber {
        self.anchor.number
    }
}

/// A collection of anchored blocks with the following properties:
///
/// * The blocks are ordered ascending by their block number
/// * The blocks are contiguous, meaning that there is no gap between the block numbers of
///   consecutive blocks in the collection. e.g. 1, 2, 3 are contiguous, but 1, 2, 4 are not.
#[derive(Debug, Clone, Default, Deref, Into, IntoIterator)]
pub struct ContiguousAnchoredBlocks(Vec<AnchoredBlock>);

impl ContiguousAnchoredBlocks {
    /// Create an empty list of `ContiguousAnchoredBlocks`
    pub const fn empty() -> Self {
        Self(Vec::new())
    }

    /// Create a new `ContiguousAnchoredBlocks` from a vector of `AnchoredBlock`s
    ///
    /// The blocks are sorted by their block number.
    pub fn new(mut blocks: Vec<AnchoredBlock>) -> Result<Self, GapError> {
        blocks.sort_by_key(|block| block.block.header.number);

        let Some(first_bn) = blocks.first().map(|b| b.block.header.number) else {
            return Ok(Self(blocks));
        };

        // check for gaps
        for (i, block) in blocks.iter().enumerate() {
            let got = block.block.header.number;
            let expected = first_bn + i as u64;

            if got != expected {
                return Err(GapError { expected, got });
            }
        }

        Ok(Self(blocks))
    }

    /// Consume this [`ContiguousAnchoredBlocks`] and return a vector of [`PreconfirmedBlock`]s
    pub fn into_preconfirmed(self) -> Vec<PreconfirmedBlock> {
        self.into_iter().map(|b| b.into_preconfirmed()).collect()
    }

    /// Convert the soft blocks into an outstanding batch that we can safely build blocks on top of.
    ///
    /// Parameters:
    /// * `safe_anchor`: to use in case the batch is empty.
    /// * `settings`: the batch settings to use.
    pub fn into_outstanding(self, safe_anchor: Header, settings: BatchSettings) -> Batch {
        // 1. Calculate the highest anchor block ID of the outstanding blocks.
        // If it's empty, use the provided safe L1 anchor number
        let highest_anchor_block = self
            .iter()
            .max_by_key(|anchored| anchored.anchor_id())
            .map(|anchored| anchored.anchor())
            .cloned()
            .unwrap_or(safe_anchor);

        // 2. filter out blocks with anchor block ID != highest_anchor_block_id.
        // We only care about blocks with the highest anchor because we can build blocks
        // on top of them in the same batch (aka our "outstanding batch").
        let mut outstanding: Vec<AnchoredBlock> = self.into();
        outstanding.retain(|anchored| anchored.anchor_id() == highest_anchor_block.number);

        // 3. Take only the last `limits.capacity` blocks
        let blocks = outstanding
            .split_off(outstanding.len().saturating_sub(settings.capacity))
            .into_iter()
            .map(|anchored| PreconfirmedBlock::from_block(anchored.into_block()))
            .collect();

        // 4. Build the batch
        Batch::from_blocks_unchecked(highest_anchor_block, settings, blocks)
    }

    /// Returns the base fee per gas for each block in the collection
    pub fn basefee_map(&self) -> HashMap<BlockNumber, u64> {
        self.iter()
            .filter_map(|b| {
                let basefee = b.block.header.base_fee_per_gas?;
                let block_number = b.block.header.number;
                Some((block_number, basefee))
            })
            .collect()
    }
}

/// An error that occurs when there is a gap in the block numbers
/// of a contiguous set of anchored blocks.
#[derive(Debug, Clone)]
pub struct GapError {
    /// The expected block number
    pub expected: BlockNumber,
    /// The found block number
    pub got: BlockNumber,
}

impl std::fmt::Display for GapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gap error: expected {} but got {}", self.expected, self.got)
    }
}
