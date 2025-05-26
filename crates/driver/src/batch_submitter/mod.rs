use alloy::rpc::types::TransactionReceipt;
use processor::{BatchSubmissionCmd, BatchSubmissionRequest};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::metrics::DriverMetrics;

pub(crate) mod errors;
pub(crate) use errors::BatchProposalError;

pub(crate) mod poster;
pub(crate) use poster::BatchPoster;

pub(crate) mod processor;
pub(crate) use processor::BatchSubmissionProcessor;

/// The maximum number of batches that can be buffered in the queue.
const MAX_BUFFERED_BATCHES: usize = 32;

/// The result type of a batch proposal.
pub(crate) type BatchResult = Result<TransactionReceipt, BatchProposalError>;

/// [`L1BatchSubmitter`] allows to submit a batch to the L1. It offers two way of doing that:
/// - a "fire-and-forget" approach, where the caller can trigger batch submission using a sender,
/// - an async approach, where the caller can await the result of the batch submission.
#[derive(Debug)]
pub(crate) struct L1BatchSubmitter {
    /// Channel to send batch submission requests.
    handle: mpsc::Sender<BatchSubmissionCmd>,
    /// Stream of results from the batch submitter.
    results_stream: Option<ReceiverStream<BatchResult>>,
}

impl L1BatchSubmitter {
    /// Take the results stream from the batch submitter.
    pub(crate) const fn take_results_stream(&mut self) -> ReceiverStream<BatchResult> {
        self.results_stream.take().expect("results stream already taken")
    }

    /// Prompt the batch submitter to submit a batch to L1.
    ///
    /// This is intentionally setup as a "fire and forget" operation. This actor
    /// will attempt to resubmit the batch in case of any errors, on every block
    /// until a deadline is reached.
    pub(crate) fn trigger_batch_submission(&self, req: BatchSubmissionRequest) {
        if req.batch().is_empty() {
            return;
        }

        DriverMetrics::increment_total_l1_batches_submitted();
        self.handle
            .try_send(BatchSubmissionCmd::Submit(req))
            .expect("failed to send batch submission request");
    }

    /// Prompt the batch submission queue to clear its submission queue.
    ///
    /// One situation where this is useful to use is when an L2 reorg is detected,
    /// because now all enqueued batches are invalid since they're built on reorged state.
    pub(crate) fn clear_batch_submissions(&self) {
        self.handle
            .try_send(BatchSubmissionCmd::ClearQueue)
            .expect("failed to send batch submission clear request");
    }

    /// Get the suggested maximum size of the next block.
    pub(crate) async fn get_suggested_max_size(&self) -> u64 {
        let (tx, rx) = oneshot::channel();
        self.handle
            .try_send(BatchSubmissionCmd::GetSuggestedMaxSize(tx))
            .expect("failed to send batch submission max size request");
        rx.await.expect("failed to receive batch submission max size")
    }
}
