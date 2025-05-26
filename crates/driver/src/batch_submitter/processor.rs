use std::{collections::VecDeque, ops::RangeInclusive, pin::Pin, task::Poll};

use alloy::{rpc::types::TransactionReceipt, transports::TransportResult};
use futures::FutureExt;
use mk1_chainio::taiko::preconf_router::PreconfRouter;
use mk1_clients::execution::ExecutionClient;
use mk1_primitives::{Slot, summary::Summary, taiko::batch::Batch};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use crate::{config::RuntimeConfig, metrics::DriverMetrics};

use super::{BatchPoster, BatchProposalError, BatchResult, L1BatchSubmitter, MAX_BUFFERED_BATCHES};

/// The minimum number of batches that must be in the queue before we start applying
/// the throttling factor to newly built blocks.
const DA_THROTTLE_THRESHOLD: usize = 1;

/// Command to be sent to the batch submission processor.
pub(crate) enum BatchSubmissionCmd {
    /// Submit a batch to propose.
    Submit(BatchSubmissionRequest),
    /// Clear the batch submission queue.
    ClearQueue,
    /// Get the suggested maximum size of the next block.
    GetSuggestedMaxSize(oneshot::Sender<u64>),
}

/// A request to submit a batch to the L1.
#[derive(Debug, Clone)]
pub(crate) struct BatchSubmissionRequest {
    /// The batch to be submitted.
    batch: Batch,
    /// The deadline for the batch to be included in a block.
    deadline: Slot,
}

impl BatchSubmissionRequest {
    pub(crate) const fn new(batch: Batch, deadline: Slot) -> Self {
        Self { batch, deadline }
    }

    pub(crate) const fn batch(&self) -> &Batch {
        &self.batch
    }
}

pub(crate) type ProposeBatchTask = Pin<Box<dyn Future<Output = BatchResult> + Send>>;

/// An actor responsible for receiving batch submission request and processing them sequentially.
pub(crate) struct BatchSubmissionProcessor {
    /// Channel to receive batch submission commands.
    request_rx: mpsc::Receiver<BatchSubmissionCmd>,
    /// Queue to hold batch submission requests.
    request_queue: VecDeque<(Batch, Slot)>,
    /// The current batch proposal task.
    pending_proposal: Option<ProposeBatchTask>,
    /// The batch poster used to submit batches to the L1.
    poster: BatchPoster,
    /// Channel to send the result of the batch proposal.
    result_tx: mpsc::Sender<BatchResult>,
    /// The configured maximum size of an L2 block transaction list, in bytes.
    max_block_size: u64,
    /// The configured DA throttling factor.
    da_throttling_factor: u64,
}

impl BatchSubmissionProcessor {
    /// Creates a new instance of [`BatchSubmissionProcessor`].
    pub(crate) async fn new(cfg: RuntimeConfig) -> TransportResult<(Self, L1BatchSubmitter)> {
        let l1_el = ExecutionClient::new(cfg.l1.el_url.clone(), cfg.l1.el_ws_url.clone()).await?;

        let preconf_router = PreconfRouter::new(
            cfg.l1.el_url.clone(),
            cfg.contracts.preconf_router,
            cfg.operator.private_key.clone(),
        );

        let max_block_size = cfg.preconf.max_block_size_bytes;
        let da_throttling_factor = cfg.preconf.da_throttling_factor;
        let poster = BatchPoster::new(cfg, l1_el, preconf_router);

        let (batch_result_tx, batch_result_rx) = mpsc::channel(MAX_BUFFERED_BATCHES);
        let (tx, rx) = mpsc::channel(MAX_BUFFERED_BATCHES);

        Ok((
            Self {
                request_rx: rx,
                request_queue: VecDeque::new(),
                pending_proposal: None,
                poster,
                max_block_size,
                da_throttling_factor,
                result_tx: batch_result_tx,
            },
            L1BatchSubmitter {
                handle: tx,
                results_stream: Some(ReceiverStream::new(batch_result_rx)),
            },
        ))
    }

    /// Calculate the suggested maximum size of a new block, based on the size of the submission
    /// queue. This is used to make sure that we aren't backlogged on L1 DA.
    fn calc_max_next_block_size(&self) -> u64 {
        let mut size = self.max_block_size;

        let queue_size = self.request_queue.len();
        if queue_size >= DA_THROTTLE_THRESHOLD {
            for _ in 0..queue_size {
                size = size.saturating_sub(size / self.da_throttling_factor);
            }
        }

        size
    }

    /// Handle the result of a batch proposal.
    fn on_proposal(&mut self, res: Result<TransactionReceipt, BatchProposalError>) {
        let queue_size = self.request_queue.len();

        match res {
            Ok(receipt) => {
                debug!(
                    "Batch proposal succeeded. queue={:?}, length={}",
                    queue_to_ranges(&self.request_queue),
                    queue_size
                );

                self.result_tx.try_send(Ok(receipt)).expect("Failed to send batch proposal result");
            }
            Err(e) => {
                if e.matches_nonce_too_low() {
                    debug!(
                        "Batch simulation failed with 'nonce too low'. Considering the first batch in queue landed"
                    );
                    return;
                }

                // All subsequent proposals will be invalid because they'll contain block
                // number which are too high, so there is no reason to keep them in the
                // queue. Clear it.
                warn!(?e, "Received error from batch proposal, cleaning up pending submission");
                DriverMetrics::increment_batch_submissions_dropped(e.to_string(), queue_size);
                self.request_queue.clear();

                self.result_tx.try_send(Err(e)).expect("Failed to send batch proposal error");
            }
        }
    }

    /// Handle a new command.
    fn on_cmd(&mut self, cmd: BatchSubmissionCmd) {
        match cmd {
            BatchSubmissionCmd::Submit(req) => {
                self.handle_batch_submission_request(req);
            }
            BatchSubmissionCmd::ClearQueue => {
                debug!("Received cmd to clear the batch submission queue");
                self.request_queue.clear();
                DriverMetrics::set_batch_submission_queue_size(0);
            }
            BatchSubmissionCmd::GetSuggestedMaxSize(tx) => {
                let max = self.calc_max_next_block_size();
                debug!("max_block_size={}b; queue_len={}", max, self.request_queue.len());
                DriverMetrics::set_suggested_max_block_size(max);
                tx.send(max).expect("failed to send batch submission max size");
            }
        }
    }

    /// Handle a new batch submission request. Logic:
    ///
    /// 1. Check if the batch overlaps with any existing batch in the queue
    /// 2. If it does, return an error
    /// 3. If it doesn't, we can add the batch to the queue safely
    /// 4. Update the queue size metric
    fn handle_batch_submission_request(&mut self, req: BatchSubmissionRequest) {
        let ranges = queue_to_ranges(&self.request_queue);

        let batch_range = req.batch.blocks_range();
        if ranges.iter().any(|r| ranges_overlap(r, &batch_range)) {
            warn!(
                batch = req.batch.summary(),
                queue = ?ranges,
                "Batch overlaps with existing batch in queue, skipping"
            );
            return;
        }

        debug!(
            position_in_queue = self.request_queue.len(),
            queue = ?ranges,
            "Adding batch to queue: {:?}",
            batch_range,
        );
        self.request_queue.push_back((req.batch, req.deadline));
        DriverMetrics::set_batch_submission_queue_size(self.request_queue.len());
    }
}

impl Future for BatchSubmissionProcessor {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            let mut progress = false;

            if let Some(proposal) = &mut this.pending_proposal {
                match proposal.poll_unpin(cx) {
                    Poll::Ready(res) => {
                        progress = true;
                        this.pending_proposal = None;
                        this.on_proposal(res);
                    }
                    Poll::Pending => { /* falltrough */ }
                }
            }

            if this.pending_proposal.is_none() {
                if let Some((batch, deadline)) = this.request_queue.pop_front() {
                    progress = true;

                    DriverMetrics::set_batch_submission_queue_size(this.request_queue.len());
                    let fut = this.poster.clone().post_batch(batch, deadline);
                    this.pending_proposal = Some(Box::pin(fut));
                }
            }

            while let Poll::Ready(Some(req)) = this.request_rx.poll_recv(cx) {
                progress = true;

                this.on_cmd(req);
            }

            if !progress {
                return Poll::Pending
            }
        }
    }
}

/// Returns the list of block ranges covered by the batches in the queue.
fn queue_to_ranges(queue: &VecDeque<(Batch, Slot)>) -> Vec<RangeInclusive<u64>> {
    queue.iter().map(|(b, _)| b.blocks_range()).collect::<Vec<_>>()
}

/// Returns true if the two ranges overlap.
fn ranges_overlap(a: &RangeInclusive<u64>, b: &RangeInclusive<u64>) -> bool {
    a.start() <= b.end() && b.start() <= a.end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ranges_overlap() {
        // Test cases for overlapping ranges
        assert!(ranges_overlap(&(1..=5), &(3..=7))); // Partial overlap right
        assert!(ranges_overlap(&(3..=7), &(1..=5))); // Partial overlap left
        assert!(ranges_overlap(&(1..=5), &(2..=4))); // One range contained within other
        assert!(ranges_overlap(&(2..=4), &(1..=5))); // One range contained within other
        assert!(ranges_overlap(&(1..=5), &(1..=5))); // Identical ranges
        assert!(ranges_overlap(&(1..=5), &(5..=10))); // Adjacent ranges (inclusive)
        assert!(ranges_overlap(&(5..=10), &(1..=5))); // Adjacent ranges (inclusive)

        // Test cases for non-overlapping ranges
        assert!(!ranges_overlap(&(1..=5), &(6..=10))); // No overlap, right side
        assert!(!ranges_overlap(&(6..=10), &(1..=5))); // No overlap, left side
        assert!(!ranges_overlap(&(1..=5), &(7..=10))); // Gap between ranges
        assert!(!ranges_overlap(&(7..=10), &(1..=5))); // Gap between ranges

        // Edge cases
        assert!(ranges_overlap(&(0..=0), &(0..=0))); // Single point ranges
        assert!(!ranges_overlap(&(0..=0), &(1..=1))); // Adjacent single points
        assert!(ranges_overlap(&(u64::MAX - 1..=u64::MAX), &(u64::MAX..=u64::MAX))); // Large numbers
    }
}
