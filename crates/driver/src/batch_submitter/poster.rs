use std::time::Duration;

use alloy::{
    consensus::{
        BlobTransactionSidecar, EthereumTxEnvelope, Transaction as _, TxEip4844Variant,
        constants::ETH_TO_WEI,
    },
    eips::{BlockId, eip2718::Encodable2718, eip4844::BYTES_PER_BLOB},
    network::TransactionBuilder,
    providers::Provider,
    rpc::types::{Block, TransactionRequest},
    transports::{RpcError, TransportResult},
};
use alloy_primitives::{B256, Bytes};
use mk1_chainio::{
    WalletProviderWithSimpleNonceManager,
    taiko::{
        inbox::ITaikoInbox::{
            BatchParams, BatchProposed, BlobParams, BlockParams, ITaikoInboxErrors,
        },
        preconf_router::PreconfRouter,
    },
    try_parse_transport_error,
};
use mk1_clients::execution::{ExecutionClient, RevertReasonTrace};
use mk1_primitives::{
    Slot,
    blob::{BlobTxFeesHints, MAX_BLOB_DATA_SIZE, create_blob_sidecar_from_data_async},
    compression::{ZlibCompressionEstimate, rlp_encode_and_compress},
    summary::Summary,
    taiko::batch::{Batch, PreconfirmedBlock},
    time::SlotClock,
};
use tokio::time::{Instant, sleep_until};
use tracing::{Level, debug, error, info, span_enabled, trace, warn};

use crate::{
    batch_submitter::errors::{BLOB_GAS_TOO_LOW_ERROR, extract_base_fee_from_err},
    config::RuntimeConfig,
    metrics::DriverMetrics,
};

use super::{
    BatchProposalError, BatchResult,
    errors::{MAX_FEE_PER_GAS_TOO_LOW_ERROR, RetryBatchSubmissionError},
};

/// The amount of time to sleep after a new L1 slot tick to ensure a `engine_newPayload` call is
/// made, plus some buffer time.
const NEW_PAYLOAD_WAIT_TIME: Duration = Duration::from_secs(6);

/// The maximum fees to be paid for a single batch proposal.
///
/// Rationale: blobs can get expensive quickly, but there should always be a
/// roof for how much we're willing to pay for inclusion. This value is far above
/// any reasonable gas spike and should be more than enough.
const MAX_FEES: u128 = ETH_TO_WEI / 8; // 0.125 ETH

/// The minimum gas limit for the batch proposal transaction.
/// This is purposefully set high to account for discrepancies between simulations and real
/// execution of the transaction.
///
/// NOTE: if we assume a `max_fee_per_gas` of 50 Gwei and a `max_fee_per_blob_gas` of 20 Gwei, with
/// a six blob transaction we have a total fee of `(5*10**6 * 50*10**9) + 6*131072 * 20*10**9) ==
/// 0.2 ETH`, below the `MAX_FEES` of 0.5 ETH.
const MIN_GAS_LIMIT: u64 = 5_000_000;

/// A struct that can carry a batch submission process to L1, using retries.
#[derive(Debug, Clone)]
pub(crate) struct BatchPoster {
    cfg: RuntimeConfig,
    l1_el: ExecutionClient,
    preconf_router: PreconfRouter,
}

impl BatchPoster {
    /// Creates a new instance of [`BatchPoster`].
    pub(crate) const fn new(
        cfg: RuntimeConfig,
        l1_el: ExecutionClient,
        preconf_router: PreconfRouter,
    ) -> Self {
        Self { cfg, l1_el, preconf_router }
    }

    /// Post a batch to L1.
    ///
    /// NOTE: consumes `self` so that the future can be sent between threads.
    pub(crate) async fn post_batch(self, batch: Batch, deadline: Slot) -> BatchResult {
        self.post_batch_inner(batch, deadline).await
    }

    /// Post a batch to L1.
    pub(crate) async fn post_batch_inner(&self, batch: Batch, deadline: Slot) -> BatchResult {
        info!("🧑‍🍳 posting batch: {}", batch.summary());

        let last_block = batch.blocks().last().expect("not empty").clone();

        let start = Instant::now();
        let (batch_params, sidecar, fees_hints) =
            self.generate_batch(batch).await.inspect_err(|e| {
                DriverMetrics::increment_batch_creation_failures(e.to_string());
            })?;

        debug!(summary = batch_params.summary(), "batch params");

        DriverMetrics::record_batch_building_time(start.elapsed());

        // We are using EIP-4844 blobs so we don't need to send the txList via calldata.
        // TODO: In the future it could be an option based on basefee/blob market price
        let tx_list = Bytes::new();

        // Post the batch to the L1 PreconfRouter contract
        let tx_req = if self.cfg.preconf.disable_expected_last_block_id {
            self.preconf_router.propose_batch_tx_request(batch_params, tx_list, sidecar)
        } else {
            self.preconf_router.propose_batch_with_expected_last_block_id_tx_request(
                batch_params,
                tx_list,
                last_block.number(),
                sidecar,
            )
        };

        let mut slot_clock = self.cfg.slot_clock();
        let result =
            self.propose_batch_with_retries(tx_req, &mut slot_clock, deadline, fees_hints).await;

        match result {
            Err(e) => {
                error!(error = ?e, "Failed to post batch");
                DriverMetrics::increment_batch_submission_failures(e.to_string());
                Err(e)
            }
            Ok(receipt) => {
                DriverMetrics::record_batch_tx_inclusion_time(start.elapsed());
                DriverMetrics::set_batch_gas_cost_in_wei(&receipt);

                if receipt.status() {
                    info!(hash = ?receipt.transaction_hash, block_number = receipt.block_number, "🦅 Batch proposal landed");
                    DriverMetrics::increment_l1_batches_included();

                    if let Some(event) = receipt.decoded_log::<BatchProposed>() {
                        sanity_check_batch_proposed_event(event.data, last_block);
                    }

                    Ok(receipt)
                } else {
                    let (hash, block_number) = (receipt.transaction_hash, receipt.block_number);

                    let l1_el = self.l1_el.clone();
                    tokio::spawn(async move {
                        let reason = l1_el.debug_revert_reason(hash).await.unwrap_or_else(|e| {
                            error!(error = ?e, ?hash, "Failed to get revert reason by tracing the tx");
                            RevertReasonTrace::Unknown
                        });

                        // Try to decode the revert reason as a contract error, and fall back
                        // to "unknown" if it fails.
                        let reason = reason.try_decode::<ITaikoInboxErrors>();

                        error!(reason, ?hash, block_number, "‼️ Batch proposal reverted");
                        DriverMetrics::increment_l1_batches_reverted(reason);
                    });

                    Err(BatchProposalError::Reverted(hash, block_number))
                }
            }
        }
    }

    /// Create blob gas hints for the batch proposal, taking into account the current gas market
    /// and the minimum parameters from the runtime configuration.
    async fn create_blob_fees_hints(&self) -> TransportResult<BlobTxFeesHints> {
        let (blob_fee_per_gas, eip1559_hints) =
            tokio::try_join!(self.l1_el.get_blob_base_fee(), self.l1_el.estimate_eip1559_fees())?;

        // We might got very unlucky and the base fees might have bumped just after making this
        // call, so better to add the maximum increase of 12.5% to the estimation.
        let fees_hints = BlobTxFeesHints::from_estimation(eip1559_hints, blob_fee_per_gas)
            .with_next_block_increase()
            .with_enforced_min_batch_tip_wei(self.cfg.preconf.min_batch_tip_wei);

        Ok(fees_hints)
    }

    /// Generate a batch ready to be sent to the L1 `PreconfRouter` contract.
    ///
    /// Returns the batch parameters, the blob sidecar and the gas hints otherwise.
    ///
    /// # Panics
    ///
    /// Panics if the batch is empty.
    async fn generate_batch(
        &self,
        batch: Batch,
    ) -> Result<(BatchParams, BlobTransactionSidecar, BlobTxFeesHints), BatchProposalError> {
        let (first_block_timestamp, last_block_timestamp) =
            batch.first_and_last_block_timestamps().expect("non-empty batch");

        let mut batch_txs = Vec::with_capacity(batch.tx_count());
        let mut blocks_params = Vec::with_capacity(batch.blocks().len());
        let raw_size = batch.raw_size();

        let batch_anchor_block = batch.anchor_id();

        let mut previous_block_timestamp = first_block_timestamp;
        for preconfirmed_block in batch.blocks() {
            let time_difference = preconfirmed_block.timestamp() - previous_block_timestamp;
            let time_shift = u8::try_from(time_difference).unwrap_or_else(|_| {
                error!(
                    "Time shift of {}s exceeds u8 range, capping at 255s. THIS IS A BUG AND WILL LEAD TO A REORG!",
                    time_difference
                );
                u8::MAX
            });

            blocks_params.push(BlockParams {
                numTransactions: preconfirmed_block.tx_list().len() as u16,
                // The time shift is the difference in seconds between the timestamp of the previous
                // and current block in the batch. The first block has time_shift=0 by definition.
                timeShift: time_shift,
                signalSlots: vec![], // TODO: implement signal slots
            });

            previous_block_timestamp = preconfirmed_block.timestamp();
            batch_txs.extend(preconfirmed_block.tx_list());
        }

        let start = Instant::now();
        let compressed_batch = rlp_encode_and_compress(&batch_txs)?;
        let compressed_size = compressed_batch.len() as u32;
        DriverMetrics::set_proposed_batch_size(raw_size, compressed_size);
        DriverMetrics::record_batch_compression_time(start.elapsed());
        ZlibCompressionEstimate::update(raw_size, compressed_size as usize);

        let sidecar = create_blob_sidecar_from_data_async(compressed_batch).await?;
        DriverMetrics::set_batch_blob_count(sidecar.blobs.len());

        let blobspace_wasted = sidecar.blobs.len() * MAX_BLOB_DATA_SIZE - compressed_size as usize;
        DriverMetrics::set_batch_blobspace_wasted(blobspace_wasted);

        debug!(
            "Batch size: raw={}b, zlib={}b, ratio={:.2}%, blob_count={}, blobspace_wasted={}b",
            raw_size,
            compressed_size,
            (f64::from(compressed_size) / raw_size as f64 * 100.0),
            sidecar.blobs.len(),
            blobspace_wasted
        );

        let batch_params = BatchParams {
            lastBlockTimestamp: last_block_timestamp,
            proposer: self.cfg.operator.private_key.address(),
            coinbase: batch.coinbase(),
            parentMetaHash: B256::ZERO,
            anchorBlockId: batch_anchor_block,
            revertIfNotFirstProposal: false,
            blobParams: BlobParams {
                byteSize: compressed_size,
                // Note that if this array is not empty, `firstBlobIndex` and `numBlobs` must be
                // 0. Reference: <https://github.com/taikoxyz/taiko-mono/blob/5e3704dfb3c85a4f2c227719b3859cd127ae8a13/packages/protocol/contracts/layer1/based/ITaikoInbox.sol#L34-L49>
                blobHashes: sidecar.versioned_hashes().collect(),
                numBlobs: 0,
                firstBlobIndex: 0,
                // Note: createdIn is only useful for forced inclusion batches, which can
                // reference previously posted blobs
                createdIn: 0,
                byteOffset: 0,
            },
            blocks: blocks_params,
        };

        let fees_hints = self.create_blob_fees_hints().await?;
        debug!("fees hints: {fees_hints}");

        Ok((batch_params, sidecar, fees_hints))
    }

    /// Propose a batch to L1 with retries. Attempts are done until a deadline is reached.
    /// It early returns in case of unrecoverable errors or if the transaction is included in a
    /// block.
    async fn propose_batch_with_retries(
        &self,
        tx_req: TransactionRequest,
        slot_clock: &mut SlotClock,
        deadline: Slot,
        mut fees_hints: BlobTxFeesHints,
    ) -> BatchResult {
        if self.cfg.current_slot() > deadline {
            return Err(BatchProposalError::DeadlineReached)
        }

        let provider = self.preconf_router.provider();

        let Some(mut latest_head) = provider.get_block(BlockId::latest()).await? else {
            return Err(BatchProposalError::NoHead)
        };

        // The initial nonce and gas limis are `None`, so that the provider can fill them and can be
        // re-used across retries.
        let mut nonce = None;
        let mut gas_limit = None;
        let mut attempts = 0;

        let batch_result = loop {
            let result =
                retry_tx_submission(provider, tx_req.clone(), fees_hints, nonce, gas_limit).await;
            attempts += 1;

            match result {
                Ok((latest_hash, latest_nonce, latest_gas_limit)) => {
                    if nonce.is_none() {
                        nonce = Some(latest_nonce);
                    }
                    if gas_limit.is_none() {
                        gas_limit = Some(latest_gas_limit);
                    }

                    latest_head = wait_for_new_block(provider, slot_clock, &latest_head).await?;

                    if let Some(receipt) = provider.get_transaction_receipt(latest_hash).await? {
                        break Ok(receipt)
                    }

                    warn!(
                        ?latest_hash,
                        ?latest_nonce,
                        attempts,
                        "batch didn't land in the latest block, retrying"
                    );
                }
                Err(RetryBatchSubmissionError::RpcError(e)) => {
                    if let RpcError::ErrorResp(ref e) = e {
                        let msg = e.message.to_lowercase();

                        if msg.contains(BLOB_GAS_TOO_LOW_ERROR) {
                            warn!(
                                "Gas hint estimation failed: blob gas too low. Retrying with higher gas: {fees_hints}"
                            );
                            fees_hints.next_block_increase();
                            continue
                        }

                        if msg.contains(MAX_FEE_PER_GAS_TOO_LOW_ERROR) {
                            let Some(new_base_fee) = extract_base_fee_from_err(&msg) else {
                                warn!(
                                    "Gas hint estimation failed: max fee per gas lower than base fee, but base fee could not be parsed from err: {msg}"
                                );
                                // We have no other choice than to retry with new hints.
                                fees_hints = self.create_blob_fees_hints().await?;
                                debug!("fees hints: {fees_hints}");
                                continue;
                            };

                            if fees_hints.bump_base_fee(new_base_fee) {
                                warn!(
                                    "Gas hint estimation failed: max fee per gas lower than base fee ({new_base_fee}). Retrying with higher gas: {fees_hints}"
                                );
                            } else {
                                // If we got this error and the base fee is already higher than the
                                // current base fee, let's retry completely.
                                fees_hints = self.create_blob_fees_hints().await?;
                                warn!(
                                    "Gas hint estimation failed: max fee per gas lower than base fee ({new_base_fee}). Retrying with new hints: {fees_hints}"
                                );
                            }
                            continue
                        }
                    }

                    // In doubt, avoid an endless loop and return the error.
                    // The driver will trigger a new sync routine if necessary.
                    break Err(RetryBatchSubmissionError::RpcError(e).into())
                }
                Err(e) => break Err(e.into()),
            }

            if self.cfg.current_slot() > deadline {
                break Err(BatchProposalError::DeadlineReached)
            }

            // Double the gas fee hints for the next attempt
            fees_hints.double();
        };

        DriverMetrics::set_batch_attempts(attempts);
        batch_result
    }
}

/// Retry the transaction submission from the given request.
///
/// NOTE: `nonce` should be `None` if it is the first attempt, so that it's filled by the provider.
async fn retry_tx_submission(
    provider: &WalletProviderWithSimpleNonceManager,
    mut tx_req: TransactionRequest,
    fees_hints: BlobTxFeesHints,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
) -> Result<(B256, u64, u64), RetryBatchSubmissionError> {
    // If the caller passes a nonce, use it
    if let Some(nonce) = nonce {
        tx_req.set_nonce(nonce);
    }
    if let Some(gas_limit) = gas_limit {
        tx_req.set_gas_limit(gas_limit);
    }

    // Update the gas hints
    tx_req.set_max_priority_fee_per_gas(fees_hints.max_priority_fee_per_gas());
    tx_req.set_max_fee_per_gas(fees_hints.max_fee_per_gas());
    tx_req = tx_req.max_fee_per_blob_gas(fees_hints.blob_fee_per_gas());

    // Note: it's critical to use a `provider` configured with `SimpleNonceManager` here,
    // otherwise we risk bricking the batch submitter with a gapped nonce error.
    let tx_envelope = match provider.fill(tx_req).await {
        Ok(filled) => filled.try_into_envelope().expect("signed transaction and wallet provider"),
        Err(err) => return Err(try_parse_transport_error::<ITaikoInboxErrors>(err).into()),
    };

    let total_fees = max_batch_fee(&tx_envelope);
    if total_fees > MAX_FEES {
        return Err(RetryBatchSubmissionError::FeesTooHigh(total_fees, MAX_FEES))
    }

    let nonce = tx_envelope.nonce();
    // Ensure a sufficiently high gas limit for the transaction to account for changes between
    // simulation and execution.
    let gas_limit = tx_envelope.gas_limit().saturating_mul(2).min(MIN_GAS_LIMIT);
    if span_enabled!(Level::DEBUG) {
        debug!(tx = %tx_envelope.summary(), "⚒️ Batch prepared");
    } else {
        info!(batch_hash = %tx_envelope.tx_hash(), %fees_hints, nonce, gas_limit, "🛠️ Batch prepared");
    }

    // Note: we use send_raw_transaction because send() is just broken.
    let pending = match provider.send_raw_transaction(&tx_envelope.encoded_2718()).await {
        Ok(pending) => pending,
        Err(err) => return Err(try_parse_transport_error::<ITaikoInboxErrors>(err).into()),
    };

    Ok((*pending.tx_hash(), nonce, gas_limit))
}

const MAX_RETRIES_FOR_NEW_BLOCK: usize = 32;

/// Wait for a new block to be mined, and update the latest head.
async fn wait_for_new_block(
    provider: &WalletProviderWithSimpleNonceManager,
    slot_clock: &mut SlotClock,
    latest_head: &Block,
) -> Result<Block, BatchProposalError> {
    trace!(block_number = latest_head.header.number, "Waiting for new block to be mined");

    for _ in 0..MAX_RETRIES_FOR_NEW_BLOCK {
        // Wait for the next L1 slot tick.
        let now = slot_clock.next_l1_tick().await;

        // Wait for the new `engine_newPayload` call to be made, since this will trigger
        // creating receipts for the transactions in such block.
        sleep_until(now + NEW_PAYLOAD_WAIT_TIME).await;

        let Some(new_head) = provider.get_block(BlockId::latest()).await? else {
            return Err(BatchProposalError::NoHead)
        };

        if new_head.header.number > latest_head.header.number {
            trace!(
                previous = latest_head.header.number,
                new = new_head.header.number,
                "New block detected, checking for batch inclusion"
            );
            return Ok(new_head)
        }

        trace!(
            block_number = ?new_head.header.number,
            "No new block detected, waiting for the next slot"
        );
    }

    Err(BatchProposalError::NoHead)
}

/// Perform sanity checks on the batch proposed event.
fn sanity_check_batch_proposed_event(event: BatchProposed, last_block: PreconfirmedBlock) {
    if event.last_block_number() != last_block.number() {
        error!(
            got = event.last_block_number(),
            expected = last_block.number(),
            "Proposed batch last block number mismatch"
        );
    }
    if event.last_block_timestamp() != last_block.timestamp() {
        error!(
            got = event.last_block_timestamp(),
            expected = last_block.timestamp(),
            "Proposed batch last block timestamp mismatch"
        );
    }
}

/// Returns the maximum fees of the given batch transaction.
///
/// Ref: <https://x.com/etherscan/status/1775507063675965589>
fn max_batch_fee(tx: &EthereumTxEnvelope<TxEip4844Variant>) -> u128 {
    let max_fee_per_gas = tx.max_fee_per_gas();
    let gas_limit = u128::from(tx.gas_limit());
    let blobs_len = tx.blob_versioned_hashes().expect("blob versioned hashes is set").len() as u128;
    let max_fee_per_blob_gas = tx.max_fee_per_blob_gas().expect("max fee per blob gas set");

    // 1. calculate blob fees as: blob gas price * blob gas used
    let blob_gas_used = blobs_len.saturating_mul(BYTES_PER_BLOB as u128);
    let blob_fees = max_fee_per_blob_gas.saturating_mul(blob_gas_used);

    // 2. calculate calldata fees as: calldata gas price * calldata gas used
    // Since we don't know the gas used yet, we use the gas limit (as an upper bound)
    let execution_fees = max_fee_per_gas.saturating_mul(gas_limit);

    // 3. sum the two values
    execution_fees.saturating_add(blob_fees)
}
