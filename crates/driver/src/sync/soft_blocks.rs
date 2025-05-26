use std::{
    collections::HashMap,
    ops::RangeInclusive,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use alloy::{
    providers::Provider,
    rpc::types::{Block, Header},
};
use alloy_primitives::BlockNumber;
use futures::{
    Stream, StreamExt,
    stream::{self, FuturesUnordered},
};
use mk1_clients::execution::{ExecutionClient, TaikoExecutionClient};
use mk1_primitives::taiko::block::{AnchoredBlock, ContiguousAnchoredBlocks};
use tracing::{debug, error};

use crate::{helpers::TaikoBlockExt, metrics::DriverMetrics, sync::SyncerError};

/// The default amount of concurrent network requests to make when fetching unsafe blocks.
const DEFAULT_CONCURRENCY: usize = 12;

/// A helper struct for downloading soft [`AnchoredBlock`]s from the L2 execution layer.
///
/// Blocks are fetched concurrently, and partitioned based on their `fee_recipient`.
/// Concurrency can be controlled with the [`DEFAULT_CONCURRENCY`] constant.
#[derive(Debug)]
pub(crate) struct SoftBlocksDownloader {
    l1_el: ExecutionClient,
    l2_el: TaikoExecutionClient,
}

impl SoftBlocksDownloader {
    /// Create a new [`SoftBlocksDownloader`] instance.
    pub(crate) const fn new(l1_el: ExecutionClient, l2_el: TaikoExecutionClient) -> Self {
        Self { l1_el, l2_el }
    }

    /// Download the provided range of blocks from the L2 execution layer.
    pub(crate) async fn download(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> Result<ContiguousAnchoredBlocks, SyncerError> {
        debug!("Downloading soft blocks: range={:?}, count={}", range, range.size_hint().0);

        let task = BlockDownloadTask::new(self.l1_el.clone(), self.l2_el.clone(), range);

        let start = Instant::now();
        let anchored_blocks = task.await?;
        let elapsed = start.elapsed();

        debug!("Downloaded {} soft blocks in {:?}", anchored_blocks.len(), elapsed);
        DriverMetrics::set_soft_blocks_count(anchored_blocks.len() as u64);
        DriverMetrics::record_soft_blocks_download_time(elapsed);

        ContiguousAnchoredBlocks::new(anchored_blocks).map_err(SyncerError::Gap)
    }
}

type BufferedStream<T> = Pin<Box<dyn Stream<Item = Result<T, SyncerError>>>>;
type AnchorFuture = Pin<Box<dyn Future<Output = Result<Header, SyncerError>>>>;

/// The task responsible for downloading the anchored blocks.
/// It splits the work into two parts:
///
/// 1. Fetching full L2 blocks
/// 2. Fetching each L2 block's corresponding anchor block header on L1
///
/// The two parts are executed concurrently, and the results are merged.
/// It returns [`Vec<AnchoredBlock>`] ordered by L2 block number.
#[must_use = "This task does nothing unless polled"]
struct BlockDownloadTask {
    l1_el: ExecutionClient,
    block_futs: BufferedStream<Block>,
    anchor_futs: FuturesUnordered<AnchorFuture>,
    anchor_cache: AnchorCache,
    out: Vec<AnchoredBlock>,
    count: usize,
}

impl BlockDownloadTask {
    /// Create a new [`BlockDownloadTask`] instance that can be polled for results.
    fn new(
        l1_el: ExecutionClient,
        l2_el: TaikoExecutionClient,
        range: RangeInclusive<BlockNumber>,
    ) -> Self {
        let count = range.size_hint().0;

        // Load the block futures
        let mut block_futs = Vec::with_capacity(count);
        for block_number in range {
            let fut = l2_el.get_block_by_number(block_number.into()).full();
            block_futs.push(async move {
                match fut.await? {
                    Some(block) => Ok(block),
                    None => Err(SyncerError::L2BlockNotFound(block_number)),
                }
            });
        }

        // Turn the futures into a buffered stream with the desired concurrency
        let block_futs = Box::pin(stream::iter(block_futs).buffered(DEFAULT_CONCURRENCY));

        Self {
            l1_el,
            count,
            block_futs,
            out: Vec::new(),
            anchor_cache: AnchorCache::default(),
            anchor_futs: FuturesUnordered::new(),
        }
    }

    /// Spawn a new anchor block fetch future and add it to the queue.
    fn spawn_anchor_block_fetch(&self, anchor_block_id: BlockNumber) {
        let fut = self.l1_el.get_block_by_number(anchor_block_id.into());
        let fut = async move {
            match fut.await? {
                Some(anchor) => Ok(anchor.header),
                None => Err(SyncerError::L1BlockNotFound(anchor_block_id)),
            }
        };

        self.anchor_futs.push(Box::pin(fut));
    }
}

impl Future for BlockDownloadTask {
    type Output = Result<Vec<AnchoredBlock>, SyncerError>;

    /// Poll the download task, fetching blocks and their corresponding anchors concurrently.
    /// Here is a high-level example of the task's execution flow:
    ///
    /// 1. A `block_fut` yields a new L2 block
    /// 2. We check if we already have the anchor for this block in our cache -> we do not, proceed
    /// 3. We spawn a new anchor block fetch future and add it to the queue
    /// 4. In the meantime, some other `block_fut`s yield new L2 blocks using the same anchor
    /// 5. We add them to the queue of blocks waiting for this anchor
    /// 6. At some point, the anchor fetch future yields: we cache the anchor and apply it to all
    ///    the blocks that were waiting for it in the queue
    /// 7. If now a `block_fut` yields another block using that anchor, we can immediately apply it
    ///    because we have it cached
    /// 8. We repeat this process until all blocks are downloaded and anchored
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            let mut progress = false;

            if let Poll::Ready(Some(result)) = this.anchor_futs.poll_next_unpin(cx) {
                let anchor = match result {
                    Ok(anchor) => anchor,
                    Err(e) => return Poll::Ready(Err(e)),
                };

                progress = true;

                // Cache the anchor, so that future blocks can use it without waiting,
                // and apply the anchor to all the L2 blocks that share the same anchor.
                let blocks = this.anchor_cache.insert_anchor(anchor.clone());
                for block in blocks {
                    this.out.push(AnchoredBlock::new(block, anchor.clone()));
                }
            }

            if let Poll::Ready(Some(result)) = this.block_futs.poll_next_unpin(cx) {
                let block = match result {
                    Ok(block) => block,
                    Err(e) => return Poll::Ready(Err(e)),
                };

                progress = true;

                let Ok(anchor_id) = block.anchor_block_id() else {
                    error!("L2 block {} has no anchor tx", block.header.number);

                    // If the block has no anchor transaction, we can't anchor it.
                    // Simply return the anchored blocks we have so far.
                    //
                    // THIS WILL (most likely) CAUSE AN L2 REORG!
                    return Poll::Ready(Ok(std::mem::take(&mut this.out)));
                };

                // Check if we have the anchor block already cached.
                match this.anchor_cache.get_mut(anchor_id) {
                    Some(AnchorState::Available(anchor)) => {
                        // If the anchor is available, we can immediately apply the block.
                        this.out.push(AnchoredBlock::new(block, anchor.clone()));
                    }
                    Some(AnchorState::Pending(blocks)) => {
                        // If the anchor is pending, add the block to the queue of blocks
                        // waiting for this anchor to be fetched.
                        blocks.push(block);
                    }
                    None => {
                        // If this anchor was never seen before, start fetching it,
                        // and add the block to the queue of blocks waiting for it.
                        this.spawn_anchor_block_fetch(anchor_id);
                        this.anchor_cache.insert_pending(anchor_id, block);
                    }
                }
            }

            if this.out.len() == this.count {
                return Poll::Ready(Ok(std::mem::take(&mut this.out)));
            }

            if !progress {
                return Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
enum AnchorState {
    /// The anchor is pending, and we're waiting for it to be fetched from L1.
    /// We store the list of blocks that are waiting for this anchor.
    Pending(Vec<Block>),
    /// The anchor is available, and we have it cached.
    Available(Header),
}

#[derive(Debug, Default)]
struct AnchorCache(HashMap<BlockNumber, AnchorState>);

impl AnchorCache {
    /// Get the anchor for a given block number, if it's available.
    fn get_mut(&mut self, block_number: BlockNumber) -> Option<&mut AnchorState> {
        self.0.get_mut(&block_number)
    }

    /// Insert a new pending block into the cache for a given anchor.
    fn insert_pending(&mut self, block_number: BlockNumber, block: Block) {
        let state = self.0.entry(block_number).or_insert_with(|| AnchorState::Pending(vec![]));
        if let AnchorState::Pending(blocks) = state {
            blocks.push(block);
        }
    }

    /// Insert a new anchor into the cache, and return the list of blocks that were waiting for it.
    fn insert_anchor(&mut self, anchor: Header) -> Vec<Block> {
        let Some(AnchorState::Pending(blocks)) = self.0.get_mut(&anchor.number) else {
            return vec![];
        };

        let blocks = std::mem::take(blocks);
        self.0.insert(anchor.number, AnchorState::Available(anchor));
        blocks
    }
}
