use std::{sync::Arc, time::Instant};

use alloy::{
    consensus::TxEnvelope,
    contract::{Error as ContractError, Result as ContractResult},
    rpc::types::Header,
    transports::{TransportError, TransportResult},
};
use mk1_chainio::taiko::anchor::{TaikoAnchor, TaikoAnchorTxInput};
use mk1_clients::{
    engine::EngineClient,
    taiko_preconf::{TaikoPreconfClient, TaikoPreconfClientError},
};
use mk1_primitives::{
    compression::rlp_encode_and_compress,
    taiko::{
        batch::PreconfirmedBlock,
        builder::{
            BlockPayload, BuildPreconfBlockRequestBody, ExecutableData, TxPoolContentParams,
        },
        constants::TAIKO_ANCHOR_V3_GAS_LIMIT,
    },
    time::Timestamp,
};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, trace, warn};

mod state;
use state::{BlockBuilderResponse, BuilderState, State};

use crate::{config::RuntimeConfig, metrics::DriverMetrics};

/// The error message returned by the L2 execution client when we're trying to build a block
/// that is lower than the current head L1 origin block ID. The full message is: "preconfirmation
/// block number (3113) is less than or equal to the current head L1 origin block ID (3225)" but we
/// trim it down to the part that doesn't change in order to detect it without resorting to regexes.
const BLOCK_ALREADY_EXISTS_ERROR: &str =
    "is less than or equal to the current head L1 origin block ID";

/// The error message returned by the Taiko Preconf client when the operator is invalid according
/// to the `PreconfWhitelist`. This may be thrown due to our sequencing window not being fully
/// synced with the one used by Taiko driver. If this happens, we can safely skip block production.
const INVALID_OPERATOR_ERROR: &str = "invalid operator";

#[derive(Debug, thiserror::Error)]
pub enum BlockBuilderError {
    #[error("RPC error: {0}")]
    Rpc(#[from] TransportError),
    #[error("Chain IO error: {0}")]
    ChainIO(#[from] ContractError),
    #[error("Zlib compression error: {0}")]
    ZlibCompression(#[from] std::io::Error),
    #[error("Taiko Preconf client error: {0}")]
    PreconfClient(#[from] TaikoPreconfClientError),
}

/// The struct responsible for building new L2 blocks on top of the L2 chain.
/// Block building from the sequencer perspective follows the following steps:
///
/// 1. Call [`Self::build_payload`] to create a new payload (without the anchor transaction)
/// 2. Call [`Self::trigger_finalize_block`] to start finalizing the given payload
///
/// The driver will receive the finalized result on the [`Self::take_blocks_produced_sub`] stream.
#[derive(Debug)]
pub(crate) struct L2BlockBuilder {
    state: BuilderState,
    inner: Arc<BlockBuilderInner>,
    block_produced_rx: Option<ReceiverStream<BlockBuilderResponse>>,
}

#[derive(Debug)]
struct BlockBuilderInner {
    cfg: RuntimeConfig,
    l2_engine: EngineClient,
    l2_preconf: TaikoPreconfClient,
    anchor: TaikoAnchor,
    guard: watch::Sender<State>,
    block_tx: mpsc::Sender<BlockBuilderResponse>,
}

impl L2BlockBuilder {
    /// Create a new [`L2BlockBuilder`] instance.
    pub(crate) async fn new(cfg: RuntimeConfig) -> TransportResult<Self> {
        let state = BuilderState::new();
        let (block_tx, block_rx) = mpsc::channel(128);

        let inner = Arc::new(BlockBuilderInner::new(cfg, state.sender(), block_tx).await?);

        Ok(Self { state, inner, block_produced_rx: Some(ReceiverStream::new(block_rx)) })
    }

    /// Take the block produced subscription.
    pub(crate) const fn take_blocks_produced_sub(
        &mut self,
    ) -> ReceiverStream<BlockBuilderResponse> {
        self.block_produced_rx.take().expect("block producer channel already taken")
    }

    /// Get the builder state.
    pub(crate) const fn state(&self) -> &BuilderState {
        &self.state
    }

    /// Get a mutable reference to the builder state.
    pub(crate) const fn state_mut(&mut self) -> &mut BuilderState {
        &mut self.state
    }

    /// Get the execution payload for a new block from the L2 execution client.
    /// Use `force` to force the production of an empty block.
    ///
    /// The result should be used to trigger block production.
    pub(crate) async fn build_payload(
        &self,
        req: BuildPayloadRequest,
    ) -> Result<Option<BlockPayload>, BlockBuilderError> {
        self.inner.build_payload(req).await
    }

    /// This function starts the process of finalizing a new L2 block in the background.
    ///
    /// The result will be sent to the channel returned by [`Self::new`].
    pub(crate) fn trigger_finalize_block(&self, anchor_block: Header, payload: BlockPayload) {
        // Set the builder state to finalizing.
        self.state.set_finalizing(payload.parent.number + 1);

        let this = Arc::clone(&self.inner);
        tokio::spawn(async move {
            match this.finalize_block(anchor_block, payload).await {
                Ok(res) => this.block_tx.send(res).await.expect("Failed to send block result"),
                Err(e) => {
                    // IMPORTANT: If the call above fails, we should release the guard manually.
                    this.guard.send(State::Idle).expect("Failed to send idle state");

                    let err_str = e.to_string();

                    if err_str.contains(BLOCK_ALREADY_EXISTS_ERROR) {
                        warn!("Block already exists, skipping finalization");
                    } else if err_str.contains(INVALID_OPERATOR_ERROR) {
                        warn!("Invalid operator, skipping block production");
                    } else {
                        error!(?e, "Error finalizing block");
                        DriverMetrics::increment_block_production_failures(err_str);
                    }
                }
            }
        });
    }

    /// Finalize a new L2 block on top of the L2 EL head.
    ///
    /// NOTE: since this function needs to be awaited and doesn't spawn a task in the background,
    /// there is no need to alter the state guard of the block builder.
    pub(crate) async fn finalize_block(
        &self,
        anchor_block: Header,
        payload: BlockPayload,
    ) -> Result<BlockBuilderResponse, BlockBuilderError> {
        self.inner.finalize_block(anchor_block, payload).await
    }
}

impl BlockBuilderInner {
    /// Create a new [`BlockBuilderInner`] instance.
    pub(crate) async fn new(
        cfg: RuntimeConfig,
        guard: watch::Sender<State>,
        block_tx: mpsc::Sender<BlockBuilderResponse>,
    ) -> TransportResult<Self> {
        let l2_engine = EngineClient::new(cfg.l2.engine_url.clone(), cfg.l2.jwt_secret);
        let l2_preconf = TaikoPreconfClient::new(
            cfg.l2.preconf_url.clone(),
            cfg.l2.preconf_ws_url.clone(),
            cfg.l2.jwt_secret,
        );

        let anchor = TaikoAnchor::new(
            cfg.l2.el_url.clone(),
            cfg.pacaya_config.chainId,
            cfg.contracts.taiko_anchor,
        );

        Ok(Self { cfg, l2_engine, l2_preconf, anchor, guard, block_tx })
    }

    /// Builds a new block payload.
    ///
    /// Payloads need to be finalized before they are considered canonical.
    ///
    /// - Returns `Ok(None)` if empty block production is disabled and the txpool is empty.
    /// - Returns `Ok(Some(BlockPayload))` if a new payload was built.
    async fn build_payload(
        &self,
        req: BuildPayloadRequest,
    ) -> Result<Option<BlockPayload>, BlockBuilderError> {
        // 1. Get the base fee for the next block
        let base_fee = if let Some(base_fee) = req.base_fee {
            base_fee
        } else {
            self.get_next_block_base_fee(req.parent.gas_used as u32, req.timestamp).await?
        };

        trace!("Base fee for block {}: {}", req.parent.number + 1, base_fee);

        // 3. Get the transaction list for the new block
        let tx_list_without_anchor_tx = if let Some(tx_list) = req.transaction_list {
            tx_list
        } else {
            let start = Instant::now();
            let max_size = req.max_size.unwrap_or(self.cfg.preconf.max_block_size_bytes);
            let tx_list = self.fetch_tx_lists(base_fee, max_size).await;
            trace!("L2 engine: fetched tx list in {:?}", start.elapsed());
            DriverMetrics::record_fetch_txs_time(start.elapsed());
            tx_list
        };

        let can_skip_empty_block =
            !(req.force_empty || req.is_last || self.cfg.preconf.enable_building_empty_blocks);

        // 4. If it's possible to skip the empty block, return `None` if there are no txs to include
        if can_skip_empty_block && tx_list_without_anchor_tx.is_empty() {
            return Ok(None);
        }

        // 5. Return the block payload ready to be finalized
        Ok(Some(BlockPayload {
            parent: req.parent,
            base_fee,
            is_last: req.is_last,
            timestamp: req.timestamp,
            tx_list_without_anchor_tx,
        }))
    }

    /// Tries to finalize a new L2 block on top of the L2 EL head by sending it to the driver.
    async fn finalize_block(
        &self,
        anchor_block: Header,
        BlockPayload { parent, base_fee, timestamp, is_last, tx_list_without_anchor_tx }: BlockPayload,
    ) -> Result<BlockBuilderResponse, BlockBuilderError> {
        let start = Instant::now();
        let anchor_tx = self.assemble_anchor_tx(&parent, &anchor_block, base_fee).await?;

        // Prepend the anchor transaction to the transaction list from the L2 engine
        let mut final_tx_list = vec![anchor_tx];
        final_tx_list.extend_from_slice(&tx_list_without_anchor_tx);

        // Encode and compress the transaction list according to the expected request format
        let compressed_encoded_tx_list = rlp_encode_and_compress(&final_tx_list)?;

        let new_block_number = parent.number + 1;
        let gas_limit =
            u64::from(self.cfg.pacaya_config.blockMaxGasLimit) + TAIKO_ANCHOR_V3_GAS_LIMIT;
        let extra_data = self.cfg.pacaya_config.baseFeeConfig.encoded_extra_data().into();
        let fee_recipient = self.cfg.operator.private_key.address();

        if is_last {
            info!("Sending EndOfSequencing mark with the last block: {}", new_block_number);
        }

        // Transform the raw tx list into a `preconfBlocks` request body
        let build_block_body = BuildPreconfBlockRequestBody {
            end_of_sequencing: is_last,
            executable_data: ExecutableData {
                transactions: compressed_encoded_tx_list,
                block_number: new_block_number,
                base_fee_per_gas: base_fee,
                parent_hash: parent.hash,
                fee_recipient,
                extra_data,
                timestamp,
                gas_limit,
            },
        };

        // Send the payload to the preconf client, and wait for a header response
        // TODO: handle failure cases here
        let start_preconf = Instant::now();
        let new_block = self.l2_preconf.build_preconf_block(&build_block_body).await?;
        trace!(elapsed = ?start_preconf.elapsed(), "Taiko preconf client: preconf block built");

        // Perform simple sanity checks on the preconf block
        sanity_check_preconf_block(&new_block, &build_block_body.executable_data);

        DriverMetrics::record_build_preconf_block_time(start_preconf.elapsed());
        DriverMetrics::record_total_block_building_time(start.elapsed());
        DriverMetrics::increment_l2_blocks_built();

        info!(
            number = new_block.number,
            time = new_block.timestamp,
            txs = final_tx_list.len(),
            anchor = anchor_block.number,
            elapsed = ?start.elapsed(),
            "🚂 Successfully produced a new L2 block"
        );

        let new_block = PreconfirmedBlock::new(&new_block, tx_list_without_anchor_tx);
        Ok(BlockBuilderResponse::new(self.guard.clone(), new_block, anchor_block))
    }

    /// Fetch the transaction list from the L2 engine.
    /// The transactions are returned as EIP-2718 bytes.
    async fn fetch_tx_lists(&self, base_fee: u64, max_bytes: u64) -> Vec<TxEnvelope> {
        let params = TxPoolContentParams {
            base_fee,
            beneficiary: self.cfg.operator.private_key.address(),
            block_max_gas_limit: u64::from(self.cfg.pacaya_config.blockMaxGasLimit),
            max_bytes_per_tx_list: max_bytes,
            max_tx_lists_per_call: self.cfg.preconf.max_tx_lists_per_call,
            min_tip: self.cfg.preconf.min_tip_wei,
            local_accounts: vec![], // TODO: add support for node-local accounts
        };

        match self.l2_engine.tx_pool_content_with_min_tip(params).await {
            Ok(Some(block_candidate)) => {
                if let Some(first_block) = block_candidate.into_iter().next() {
                    return first_block.tx_list;
                }
            }
            Ok(None) => debug!("L2 engine returned no block candidate. Producing an empty list."),
            Err(e) => error!("Failed to fetch transaction lists from L2 engine: {e}"),
        }

        Vec::new()
    }

    /// Prepare and sign the system anchor transaction for the next block.
    async fn assemble_anchor_tx(
        &self,
        parent_block: &Header,
        anchor_block: &Header,
        base_fee_per_gas: u64,
    ) -> TransportResult<TxEnvelope> {
        let input = TaikoAnchorTxInput {
            anchor_block_id: anchor_block.number,
            anchor_state_root: anchor_block.state_root,
            parent_gas_used: parent_block.gas_used as u32,
            base_fee_config: self.cfg.pacaya_config.baseFeeConfig.into(),
            signal_slots: vec![], // TODO: implement this eventually
        };

        self.anchor.build_anchor_v3_tx(input, parent_block.hash, u128::from(base_fee_per_gas)).await
    }

    /// Get the base fee for the next block.
    async fn get_next_block_base_fee(
        &self,
        parent_gas_used: u32,
        next_block_ts: Timestamp,
    ) -> ContractResult<u64> {
        let base_fee_cfg = self.cfg.pacaya_config.baseFeeConfig.into();
        let res = self.anchor.get_base_fee_v2(parent_gas_used, next_block_ts, base_fee_cfg).await?;

        Ok(res.base_fee.to())
    }
}

/// Request for building new L2 block payloads.
#[derive(Debug, Clone)]
pub(crate) struct BuildPayloadRequest {
    /// The timestamp of the new block.
    timestamp: Timestamp,
    /// If true, force the production of an empty block.
    force_empty: bool,
    /// If true, the new block is the last one in the current sequencing window, and
    /// will relay the `EndOfSequencing` mark to the Taiko preconf client.
    is_last: bool,
    /// The parent block header to use for the new payload.
    parent: Header,
    /// The list of transactions to include in the new payload.
    /// If not provided, it will be fetched from the L2 engine.
    transaction_list: Option<Vec<TxEnvelope>>,
    /// The suggested base fee for the new block.
    base_fee: Option<u64>,
    /// The maximum size of the new block, in bytes.
    /// If not provided, the default configured value will be used.
    max_size: Option<u64>,
}

impl BuildPayloadRequest {
    /// Create a new request for building a new L2 block.
    pub(crate) const fn new(timestamp: Timestamp, parent: Header) -> Self {
        Self {
            timestamp,
            force_empty: false,
            is_last: false,
            parent,
            transaction_list: None,
            base_fee: None,
            max_size: None,
        }
    }

    /// Set the list of transactions to include in the new payload.
    pub(crate) fn with_transaction_list(mut self, transaction_list: Vec<TxEnvelope>) -> Self {
        self.transaction_list = Some(transaction_list);
        self
    }

    /// Set the flag to force the production of an empty block.
    pub(crate) const fn with_force_empty(mut self, force_empty: bool) -> Self {
        self.force_empty = force_empty;
        self
    }

    /// Set the flag to indicate that the new block is the last one in the sequencing window.
    pub(crate) const fn with_is_last(mut self, is_last: bool) -> Self {
        self.is_last = is_last;
        self
    }

    /// Set the maximum size of the new block, in bytes
    pub(crate) const fn with_max_size(mut self, max_size: u64) -> Self {
        self.max_size = Some(max_size);
        self
    }

    /// Set the suggested base fee for the new block, in wei
    pub(crate) const fn with_base_fee(mut self, base_fee: u64) -> Self {
        self.base_fee = Some(base_fee);
        self
    }
}

/// Perform a sanity check on the preconf block.
fn sanity_check_preconf_block(new_block: &Header, expected: &ExecutableData) {
    if new_block.timestamp != expected.timestamp {
        error!(
            "Preconfirmed block timestamp mismatch: preconf={}, expected={}",
            new_block.timestamp, expected.timestamp
        );
    }
    if new_block.base_fee_per_gas.is_some_and(|bf| bf != expected.base_fee_per_gas) {
        error!(
            "Preconfirmed block base fee mismatch: preconf={}, expected={}",
            new_block.base_fee_per_gas.unwrap_or_default(),
            expected.base_fee_per_gas
        );
    }
    if new_block.gas_limit != expected.gas_limit {
        error!(
            "Preconfirmed block gas limit mismatch: preconf={}, expected={}",
            new_block.gas_limit, expected.gas_limit
        );
    }
    if new_block.extra_data != expected.extra_data {
        error!(
            "Preconfirmed block extra data mismatch: preconf={}, expected={}",
            new_block.extra_data, expected.extra_data
        );
    }
    if new_block.beneficiary != expected.fee_recipient {
        error!(
            "Preconfirmed block beneficiary mismatch: preconf={}, expected={}",
            new_block.beneficiary, expected.fee_recipient
        );
    }
    if new_block.number != expected.block_number {
        error!(
            "Preconfirmed block number mismatch: preconf={}, expected={}",
            new_block.number, expected.block_number
        );
    }
    if new_block.parent_hash != expected.parent_hash {
        error!(
            "Preconfirmed block parent hash mismatch: preconf={}, expected={}",
            new_block.parent_hash, expected.parent_hash
        );
    }
}
