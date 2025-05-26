use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy::{
    contract::Error as ContractError,
    eips::merge::EPOCH_SLOTS,
    rpc::types::{Block, BlockTransactionsKind, Header, Log},
    transports::TransportError,
};
use alloy_primitives::BlockNumber;
use mk1_chainio::taiko::inbox::ITaikoInbox::BatchProposed;
use mk1_config::Opts;
use mk1_primitives::{
    Slot, SlotUtils,
    summary::Summary,
    taiko::{
        batch::{AddBlockResult, Batch, OrderedBatches, PreconfirmedBlock},
        batch_map::ReorgReasons,
        builder::TaikoPreconfWsEvent,
    },
    task::CriticalTasks,
    time::{SlotTick, Timestamp, current_timestamp_ms, current_timestamp_seconds},
};
use thiserror::Error;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, trace, warn};

use crate::{
    batch_submitter::{
        BatchProposalError, BatchSubmissionProcessor, L1BatchSubmitter,
        processor::BatchSubmissionRequest,
    },
    block_builder::{BlockBuilderError, BuildPayloadRequest, L2BlockBuilder},
    config::{RuntimeConfig, RuntimeConfigError},
    helpers::{self, TaikoBlockExt as _},
    lookahead::Lookahead,
    metrics::DriverMetrics,
    state::{L1State, L2State},
    status::DriverStatus,
    sync::{Syncer, SyncerError},
};

/// The errors that can occur during the driver's operation.
/// Note that these errors won't halt the event loop in any case once it has started.
#[derive(Debug, Error)]
pub enum DriverError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("Error in runtime configuration: {0}")]
    RuntimeConfig(#[from] RuntimeConfigError),
    #[error("Error while syncing the driver: {0}")]
    Syncer(#[from] SyncerError),
    #[error("Insufficient token balance: {0}")]
    InsufficientTokenBalance(String),
    #[error("Contract error: {0}")]
    Contract(#[from] ContractError),
    #[error("Error while proposing batch: {0}")]
    BatchProposal(#[from] BatchProposalError),
    #[error("Error while building block: {0}")]
    BlockBuilderError(#[from] BlockBuilderError),
    #[error("Driver error: {0}")]
    Custom(String),
}

/// Mk1 sequencer driver.
///
/// The driver manages the main execution loop, responsible for:
/// - Producing new L2 blocks when it is the active sequencer
/// - Proposing batches to L1 at the end of its sequencing window
///
/// To fulfill these duties, it needs context from both L1 and L2 providers, as
/// well as the Taiko preconf client.
#[derive(Debug)]
pub struct Driver {
    /// Driver runtime configuration.
    cfg: RuntimeConfig,
    /// Layer 1 chain state
    l1: L1State,
    /// Layer 2 chain state
    l2: L2State,
    /// The current operational status of the driver.
    status: DriverStatus,
    /// Component responsible for syncing unsafe blocks and batches.
    syncer: Syncer,
    /// Component responsible for building L2 blocks.
    block_builder: L2BlockBuilder,
    /// Component responsible for submitting batches to L1.
    batch_submitter: L1BatchSubmitter,
    /// Component responsible for controlling the `PreconfWhitelist`.
    lookahead: Lookahead,
    /// List of L2 blocks that have been produced and are waiting to be sent to L1.
    /// Each block also contains the original `txList` that was used to generate it.
    outstanding_batch: Batch,
    /// Long-running tasks manager
    tasks: CriticalTasks,
}

impl Driver {
    /// Create a new [`Driver`] instance.
    pub async fn new(opts: Opts) -> Result<Self, DriverError> {
        let cfg = RuntimeConfig::from_opts(opts).await?;

        info!("{}", cfg.summary());

        // Wait for the L1 and L2 execution clients to sync before proceeding
        helpers::wait_for_clients_sync(&cfg).await?;
        info!("L1 and L2 execution clients are synced!");

        // Fetch chain state
        let l1 = L1State::new(cfg.clone()).await?;
        let l2 = L2State::new(&cfg).await?;

        let mut tasks = CriticalTasks::new();

        // Build the components that will be used by the driver
        let lookahead = Lookahead::new(cfg.clone());
        let (processor, batch_submitter) = BatchSubmissionProcessor::new(cfg.clone()).await?;
        tasks.add_task(processor, "Batch submission processor");

        let block_builder = L2BlockBuilder::new(cfg.clone()).await?;
        let syncer = Syncer::new(cfg.clone()).await?;

        // Check if our operator is in the whitelist.
        let operator = cfg.operator.private_key.address();
        if !lookahead.is_whitelisted().await {
            warn!(%operator, "Operator address is not whitelisted");
        }

        // Check if we have sufficient TAIKO and ETH
        l1.assert_token_balances().await?;

        // Check and handle TAIKO token approval
        l1.approve_taiko_spending().await?;

        info!(%operator, "Token balances and approvals verified");

        let outstanding_batch = Batch::new(l1.anchor.clone(), cfg.batch_settings());
        let status = DriverStatus::Inactive;
        DriverMetrics::set_driver_status(status);

        Ok(Self {
            l1,
            l2,
            cfg,
            syncer,
            tasks,
            status,
            lookahead,
            block_builder,
            batch_submitter,
            outstanding_batch,
        })
    }

    /// Perform a startup sync of the driver.
    ///
    /// NOTE: retries submission of the unsafe batches until it succeeds.
    ///
    /// See [`Self::sync_inner`] for more details.
    pub async fn startup_sync(mut self) -> Self {
        DriverMetrics::set_mk1_version(env!("CARGO_PKG_VERSION").to_owned());

        let mut sync_closure = async || {
            // Sync the state with the latest L1 block
            let l1_tip = self.l1.el.get_header(None).await?;
            self.update_l1_info(l1_tip).await?;

            // Sync the outstanding batch and submit any unsafe blocks
            self.sync_and_propose().await;

            info!("✅ Driver synced successfully");
            Ok::<(), DriverError>(())
        };

        loop {
            // Retry the sync until it succeeds
            match sync_closure().await {
                Ok(()) => return self,
                Err(e) => {
                    error!(error = ?e, "Error while syncing the driver, retrying...");
                    sleep(Duration::from_secs(3)).await;
                }
            }
        }
    }

    /// Performs the following critical tasks to ensure the driver syncs to a valid state:
    /// 1. Syncs the L1 state and the lookahead with the latest block
    /// 2. Fetches all the soft blocks and groups them into batches
    /// 3. If such batches have been re-anchored or re-timestamped, it reposts its blocks on the
    ///    P2P.
    /// 4. Submits all unsafe batches to the L1.
    /// 4. The outstanding batch is updated with the new blocks.
    ///
    /// NOTE: this process is done in loop until it succeeds.
    async fn sync_and_propose(&mut self) {
        let (mut batches, reorg_reasons, basefee_map) = loop {
            let driver_status = self.lookahead.sync(self.l1.head_slot).await;
            debug!(status = ?driver_status, "Syncing outstanding batches");
            self.update_status(driver_status);

            if self.block_builder.state().is_finalizing().is_some() {
                self.block_builder.state_mut().wait_for_idle().await;

                // A finalized L2 block has been successfully sent to the p2p,
                // as such we can safely update our `head_block` later.
            }

            match self.syncer.sync(self.status).await {
                Ok(result) => break result,
                Err(e) => {
                    error!(?e, "Error while performing startup sync routine; retrying...");
                    sleep(Duration::from_secs(3)).await;
                }
            }
        };

        let pop_ob = |batches: &mut OrderedBatches| -> Batch {
            batches
                .pop_with_coinbase(self.cfg.operator.private_key.address())
                .unwrap_or(Batch::new(self.l1.anchor.clone(), self.cfg.batch_settings()))
        };

        let outstanding_batch = if self.status.can_propose() {
            if !reorg_reasons.is_empty() {
                warn!(
                    ?reorg_reasons,
                    "Re-posting blocks to P2P. This will cause a L2 reorg immediately"
                );
                if let Err(e) = self
                    .finalize_all_blocks_from_batches(&batches, reorg_reasons, basefee_map)
                    .await
                {
                    error!(?e, "Error while finalizing blocks from batches");
                } else {
                    info!("Posted all re-anchored/timestamped blocks to P2P");
                }
            }

            let outstanding_batch = pop_ob(&mut batches);
            if !batches.is_empty() {
                info!("Triggering proposal of {} unsafe batches", batches.len());
            }

            for batch in batches {
                DriverMetrics::increment_unsafe_batches_submitted(batch.summary());
                let deadline = self.lookahead.inclusion_window_end();
                let req = BatchSubmissionRequest::new(batch, deadline);
                self.batch_submitter.trigger_batch_submission(req);
            }

            outstanding_batch
        } else {
            pop_ob(&mut batches)
        };

        info!(
            anchor_age_blocks = self.l1.tip.number - outstanding_batch.anchor_id(),
            anchor_age_time = current_timestamp_seconds() - outstanding_batch.anchor().timestamp,
            oldest_timestamp = ?outstanding_batch.first_timestamp().map(|ts| current_timestamp_seconds() - ts),
            "Outstanding batch synced"
        );

        let old_range = self.outstanding_batch.blocks_range();
        let new_range = outstanding_batch.blocks_range();
        if old_range != new_range {
            warn!(?old_range, ?new_range, "Outstanding batch blocks range changed");
        }

        self.outstanding_batch = outstanding_batch;
        DriverMetrics::set_outstanding_batch_blocks_count(self.outstanding_batch.blocks().len());
    }

    /// Start the sequencer driver event loop.
    ///
    /// This function will run until the driver is stopped.
    pub async fn start(mut self) -> ! {
        // Create an interval that ticks at every L1 and L2 block timestamp
        let mut slot_clock = self.cfg.slot_clock();

        // Subscribe to new L1 and L2 blocks
        let (mut l1_block_sub, handle) = self.l1.el.subscribe_blocks(BlockTransactionsKind::Hashes);
        self.tasks.add_handle(handle, "L1 block sub");
        let (mut l2_block_sub, handle) = self.l2.el.subscribe_blocks(BlockTransactionsKind::Full);
        self.tasks.add_handle(handle, "L2 block sub");

        // Subscribe to new L1 batch proposal events
        let filter = self.l1.taiko_inbox.batch_proposed_filter();
        let mut batch_events = self.l1.el.subscribe_log_event::<BatchProposed>(filter);

        // Subscribe to operator changes in `PreconfWhitelist`
        let filter = self.l1.preconf_whitelist.operator_changed_filter();
        let mut operator_changed_event = self.l1.el.subscribe_events(filter);

        // Subscribe to Taiko preconf Websocket events
        let mut taiko_preconf_events = self.l2.preconf_client.subscribe_ws_events();

        // Take the block produced subscription from the `L2BlockBuilder`
        let mut blocks_produced_sub = self.block_builder.take_blocks_produced_sub();

        // Take the results stream from the batch submitter
        let mut batch_results_stream = self.batch_submitter.take_results_stream();

        info!("🤠 Starting driver event loop");

        loop {
            tokio::select! {
                biased;

                // Handle new L1 and L2 slot ticks
                Some(slot_tick) = slot_clock.next() => {
                    match slot_tick {
                        SlotTick::L1(timestamp) => self.on_l1_slot_tick(timestamp).await,
                        SlotTick::L2(timestamp) => self.on_l2_slot_tick(timestamp).await,
                    }
                }

                // Handle new Taiko preconf Websocket events
                Some(taiko_preconf_event) = taiko_preconf_events.next() => {
                    self.on_taiko_preconf_event(taiko_preconf_event).await;
                }

                // Handle new block building results from the `L2BlockBuilder`
                Some(block_builder_result) = blocks_produced_sub.next() => {
                    let (new_block, anchor) = block_builder_result.into_block_with_anchor();
                    self.on_produced_block(new_block, anchor).await;
                }

                // Handle new L1 blocks that have been added to the L1 EL chain
                Some(l1_block) = l1_block_sub.next() => {
                    self.on_l1_block(l1_block).await;
                }

                // Handle new L2 blocks that have been added to the L2 EL chain
                Some(l2_block) = l2_block_sub.next() => {
                    self.on_l2_block(l2_block).await;
                }

                // Handle a change in the operator set in the `PreconfWhitelist` contract
                Some(_) = operator_changed_event.next() => {
                    self.on_operator_changed().await;
                }

                // Handle new batch proposal events
                Some((log, batch_proposed)) = batch_events.next() => {
                    self.on_batch_proposed(log, batch_proposed);
                }

                // Handle new batch submission results
                Some(Err(e)) = batch_results_stream.next() => {
                    self.on_batch_proposal_err(e).await;
                }

                Some(task_res) = &mut self.tasks => {
                    let name = task_res.name().to_owned();
                    let reason = task_res.error_message();
                    panic!("Critical task {} failed: {}", name, reason);
                }
            }
        }
    }

    /// Handle a new L1 slot tick.
    ///
    /// We update the current head slot to a value derived from the timestamp, independently from
    /// any L1 blocks (which may or may not be missed). We also check if we are in the handover
    /// window, and if so, we submit the outstanding batch before the window closes.
    async fn on_l1_slot_tick(&mut self, timestamp: Timestamp) {
        let slot = self.cfg.timestamp_to_l1_slot(timestamp);
        info!(slot, timestamp, "⏰ L1 slot tick");

        let next_block_ts = timestamp + self.cfg.chain.l1_block_time;
        let next_block_num = self.l1.tip.number + 1;

        // Update the L1 head slot
        self.l1.head_slot = slot;
        DriverMetrics::set_l1_head_slot(slot);

        info!("📥 Outstanding batch: {}", self.outstanding_batch.summary());

        // If the outstanding batch anchor is getting close to the age limit, trigger a proposal.
        // We use a buffer of a few slots to account for the time it takes for batches to land.
        let slots_left = self.outstanding_batch.compute_age_limit(next_block_ts, next_block_num);

        if slots_left <= self.cfg.preconf.anchor_max_height_buffer {
            info!(
                blocks = self.outstanding_batch.blocks().len(),
                "👵🏻 Outstanding batch anchor reached the age limit. Proposing batch to L1"
            );

            let deadline = self.cfg.current_slot() + slots_left;
            self.submit_local_outstanding_batch(deadline).await;
        }

        if slot % EPOCH_SLOTS == 0 && self.lookahead.is_new_inclusion_window(slot) {
            info!("🔑 New inclusion window has started. Submitting unsafe blocks to L1");
            self.sync_and_propose().await;
        } else {
            // just update our status
            self.update_status(self.lookahead.driver_status(slot));
        }

        // This means we have [`RuntimeConfig::handover_window_slots`] slots left
        // for landing the last L1 batch and we should start now.
        if self.status.is_inclusion_only() {
            debug!("Handover window: attempting to submit outstanding batch");

            let deadline = self.lookahead.inclusion_window_end();
            self.submit_local_outstanding_batch(deadline).await;
        }

        // Update the anchor block of the outstanding batch if it's empty.
        self.update_outstanding_batch_anchor_if_needed();
    }

    /// Handle a new L2 slot tick.
    ///
    /// We check if we are in the sequencing window and if so, we start block
    /// production in the background.
    async fn on_l2_slot_tick(&self, timestamp: Timestamp) {
        if !self.status.can_sequence() {
            debug!(status = ?self.status, "💤 Cannot sequence");
            return;
        }

        debug!(timestamp, "⌚️ L2 slot tick. Starting L2 block production");

        let slots_left = self.lookahead.sequencing_slots_left(self.l1.head_slot);
        DriverMetrics::set_sequencing_slots_left_in_window(slots_left);

        // Ensure that we don't produce a block while a previous block is being finalized.
        // Otherwise we'll build blocks on the same parent, resulting in reorgs.
        if let Some(block_number) = self.block_builder.state().is_finalizing() {
            warn!(block_number, "L2 block is being finalized, skipping block production");
            DriverMetrics::increment_skipped_blocks_due_to_finalization_lag(block_number);
            return;
        }

        let force = if self.l2.head_block == 0 {
            // If the L2 is at genesis, we need to build an initial block to be able to send txs.
            info!("L2 chain is at genesis, forcing block production (even if empty)");
            true
        } else {
            // Check if the last block in the outstanding batch risks causing a timeshift that
            // is too high. If so, we need to produce a new empty block to reset the timeshift,
            // even if it might be empty.
            self.outstanding_batch.should_build_empty_block(timestamp)
        };

        // Perf: fetch the L2 parent block and the highest unsafe block info concurrently.
        let (parent, highest_unsafe_block) =
            match tokio::join!(self.l2.el.get_header(None), self.l2.preconf_client.get_status()) {
                (Ok(parent), Ok(status)) => (parent, status.highest_unsafe_l2_payload_block_id),
                res => {
                    error!(?res, "Error while fetching L2 parent block or Taiko preconf status");
                    return;
                }
            };

        debug!(geth = parent.number, driver = highest_unsafe_block, "L2 chain height");

        // INVARIANT: the L2 chain tip will always be the highest of the unsafe tip OR the
        // head L1 origin. This covers all cases where we can or cannot build a block. Examples:
        // - No reorgs (happy case): highest_unsafe_block is tracking the current tip
        // - L2 p2p reorg: highest_unsafe_block is tracking the new tip, so we can build on it
        // - L1 BatchProposed-triggered L2 reorg: no new unsafe blocks on p2p, but head_l1_origin is
        //   bumped by the new batch, and it is tracking the new tip.
        let l2_tip = highest_unsafe_block.max(self.l2.head_l1_origin);

        if parent.number < l2_tip {
            warn!(
                parent = parent.number,
                highest_unsafe_block,
                head_l1_origin = self.l2.head_l1_origin,
                l2_tip,
                "Trying to use a parent block lower than the L2 tip; skipping block production"
            );
            DriverMetrics::increment_skipped_blocks_due_to_parent_block_too_low(
                parent.number,
                highest_unsafe_block,
            );
            return;
        }

        let is_last = self.lookahead.is_last_block_in_sequencing_window(timestamp);

        // Query the batch submitter for the maximum size of the next block. This allows us to
        // build blocks based on how hard we are falling behind with L1 DA, which is a bottleneck.
        let max_size = self.batch_submitter.get_suggested_max_size().await;

        let req = BuildPayloadRequest::new(timestamp, parent)
            .with_force_empty(force)
            .with_is_last(is_last)
            .with_max_size(max_size);

        match self.block_builder.build_payload(req).await {
            Ok(Some(payload)) => {
                let anchor_block =
                    match self.outstanding_batch.doesnt_fit(&payload.tx_list_without_anchor_tx) {
                        Some(reason) => {
                            debug!(
                                ?reason,
                                "Outstanding batch doesn't fit the new block, using new anchor"
                            );
                            self.l1.anchor.clone()
                        }
                        None => self.outstanding_batch.anchor().clone(),
                    };
                // Try to add the block to the canonical L2 chain. This will set
                // the builder state to `Finalizing` until done.
                self.block_builder.trigger_finalize_block(anchor_block, payload);
            }
            Ok(None) => info!("No transactions to include; block production skipped"),
            Err(e) => error!(?e, "Error building block payload"),
        }
    }

    /// Handle new events from taiko-driver websocket server.
    async fn on_taiko_preconf_event(&mut self, event: TaikoPreconfWsEvent) {
        match event {
            TaikoPreconfWsEvent::EndOfSequencing(msg) => {
                info!(epoch = msg.current_epoch, "Received end of sequencing event");
                self.lookahead.update_end_of_sequencing_mark(msg.current_epoch);

                // Update the driver status
                let status = self.lookahead.sync(self.l1.head_slot).await;
                self.update_status(status);
            }
        }
    }

    /// Handles a new block produced by the [`L2BlockBuilder`].
    ///
    /// We add the new block to the outstanding batch, and if the batch is full,
    /// we trigger a proposal if we're in the inclusion window, or defer it otherwise.
    async fn on_produced_block(&mut self, new_block: PreconfirmedBlock, anchor: Header) {
        debug!("Built new block. num={}, ts={}", new_block.number(), new_block.timestamp());

        let expected = self.outstanding_batch.blocks_range().end().saturating_add(1);
        let got = new_block.number();

        // If got == expected, this will be empty because it's an `InclusiveRange`.
        let gap = expected..=got.saturating_sub(1);

        let mut blocks_to_add = if self.outstanding_batch.is_empty() || gap.is_empty() {
            vec![]
        } else {
            warn!(?gap, "Detected a gap in the outstanding batch; downloading missing blocks");
            DriverMetrics::increment_batch_block_gaps(expected.saturating_sub(got));
            match self.syncer.download_anchored_blocks(gap).await {
                Ok(gapped_blocks) => gapped_blocks.into_preconfirmed(),
                Err(e) => {
                    error!(?e, "Error while downloading gapped blocks. This will cause a reorg");
                    DriverMetrics::increment_batch_block_gaps_fill_failures(e.to_string());
                    vec![]
                }
            }
        };

        // Add the new block to the end of the list of blocks to add to the
        // outstanding batch, after the gap blocks, if any.
        blocks_to_add.push(new_block);

        for block in blocks_to_add {
            let full_batch =
                match self.outstanding_batch.add_block_and_reset_if_full(block, &anchor) {
                    AddBlockResult::Added { raw_size } => {
                        let compressed_size_rx = self.outstanding_batch.trigger_compression();
                        helpers::compute_batch_size_metrics(raw_size, compressed_size_rx);
                        continue;
                    }
                    AddBlockResult::Full { reason, batch } => {
                        info!(?reason, "🍔 Batch is full");
                        batch
                    }
                };

            let deadline = self.lookahead.inclusion_window_end();
            if self.status.can_propose() {
                let req = BatchSubmissionRequest::new(full_batch, deadline);
                self.batch_submitter.trigger_batch_submission(req);
            } else {
                debug!(
                    "Cannot propose outstanding batch, will do with the start of inclusion window sync"
                );
            }
        }

        DriverMetrics::set_outstanding_batch_blocks_count(self.outstanding_batch.blocks().len());
    }

    /// Handle a new L1 block.
    ///
    /// We update the cached L1 head block number and timestamp, and the cached L1 anchor block.
    /// We also update the cached L2 head L1 origin.
    async fn on_l1_block(&mut self, l1_block: Block) {
        info!(num = l1_block.header.number, "📦 Received new L1 block");
        DriverMetrics::set_l1_head_number(l1_block.header.number);

        let anchor_age = l1_block.header.number.saturating_sub(self.outstanding_batch.anchor_id());
        DriverMetrics::set_l2_anchor_block_age(anchor_age);

        match self.update_l1_info(l1_block.header).await {
            Ok(slot) => {
                if self.l1.is_new_epoch(slot) {
                    let epoch = slot.to_epoch();
                    self.l1.epoch = epoch;
                    info!(epoch, slot, "⚙️ New epoch has started");
                }
            }
            Err(e) => error!(?e, "Error while updating L1 info"),
        }
    }

    /// Handle a new L2 block that has been added to the L2 EL chain.
    ///
    /// We check for reorgs, and update the cached L2 head block number.
    /// If necessary, we also update the cached L1 anchor.
    async fn on_l2_block(&mut self, l2_block: Block) {
        debug!(
            "Received new L2 block from EL. num={}, ts={}",
            l2_block.header.number, l2_block.header.timestamp
        );
        let now_ms = current_timestamp_ms();

        let built_by_us = l2_block.header.beneficiary == self.cfg.operator.private_key.address();

        DriverMetrics::set_l2_el_head(l2_block.header.number);
        DriverMetrics::set_block_tx_count(l2_block.transactions.len() as u64, built_by_us);
        DriverMetrics::set_block_gas_used(l2_block.header.gas_used, built_by_us);

        if l2_block.header.number < self.l2.head_block {
            warn!("📍 L2 reorg detected: {} -> {}", self.l2.head_block, l2_block.header.number);
            debug!(
                number = l2_block.header.number,
                hash = %l2_block.header.hash,
                parent_hash = %l2_block.header.parent_hash,
                fee_recipient = %l2_block.header.beneficiary,
                "Reorged L2 block info"
            );
            DriverMetrics::increment_l2_reorgs(l2_block.header.number, self.l2.head_block);

            // Make sure to don't post other batches because they're built on reorged state.
            self.batch_submitter.clear_batch_submissions();

            // Make sure to clean outstanding batch from reorged blocks.
            self.outstanding_batch.retain_blocks_before(l2_block.header.number);

            // Make sure the head L1 origin is up to date.
            if let Err(e) = self.update_head_l1_origin().await {
                error!(?e, "Error while updating L1 origin during L2 block reorg");
            }
        }

        self.l2.head_block = l2_block.header.number;

        if built_by_us {
            trace!(
                number = %l2_block.header.number,
                txs = l2_block.transactions.len(),
                "📢 Received unsafe L2 block built by our operator; skipping"
            );
            return;
        }

        debug!(
            number = %l2_block.header.number,
            fee_recipient = %l2_block.header.beneficiary,
            txs = l2_block.transactions.len(),
            gas = l2_block.header.gas_used,
            delay_ms = now_ms.saturating_sub(u128::from(l2_block.header.timestamp) * 1000),
            "📢 Received new L2 block"
        );

        let anchor = match l2_block.anchor_block_id() {
            Ok(anchor) => anchor,
            Err(e) => {
                error!(?e, "error while getting anchor block ID from latest L2 block");
                return;
            }
        };

        if anchor > self.l1.anchor.number {
            debug!(
                old = self.l1.anchor.number,
                new = anchor,
                "Updating cached anchor block from incoming unsafe block with higher anchor"
            );

            if let Err(e) = self.l1.update_anchor(anchor).await {
                error!(?e, "Error while updating L1 anchor block");
            }
        }
    }

    /// Handles a change in the operator whitelist. If this happens mid-epoch, it is important
    /// to re-sync and trigger submission of unsafe batches, otherwise the driver might
    /// skip blocks when entering the handover window.
    async fn on_operator_changed(&mut self) {
        info!("Operator whitelist changed, re-syncing");
        self.sync_and_propose().await;
    }

    /// Handles a new batch proposed event from the Taiko inbox.
    fn on_batch_proposed(&self, raw_log: Log, batch: BatchProposed) {
        debug!(
            block_number=raw_log.block_number,
            block_hash=?raw_log.block_hash,
            tx_hash=?raw_log.transaction_hash,
            "🗞️ Received new Taiko inbox batch. Summary: {}",
            batch.summary(),
        );
    }

    async fn on_batch_proposal_err(&mut self, e: BatchProposalError) {
        debug!("Handling batch proposal error");

        // Ignore the case when we're not the preconfer. We can't do anything about it:
        // - if our inclusion window is done, we can't propose;
        // - if we're only sequencing, when our inclusion window starts a sync will be triggered,
        //   attempting to propose any batches before continuing.
        if e.matches_not_preconfer() {
            debug!("Ignoring batch submission error due to not being the active preconfer");
            return;
        }

        // Try to propose all the unsafe batches
        self.sync_and_propose().await;
    }

    /// Updates all L1-related information needed by the driver upon receiving a new L1 block.
    /// Returns the slot corresponding to the L1 block received from the EL.
    async fn update_l1_info(&mut self, l1_header: Header) -> Result<Slot, DriverError> {
        let slot = self.cfg.timestamp_to_l1_slot(l1_header.timestamp);

        // Sync the full L1 state
        self.l1.sync(l1_header).await?;

        // Update the head L1 origin from the protocol
        self.update_head_l1_origin().await?;

        // Try to remove blocks from the outstanding batch that are now safe
        self.outstanding_batch.retain_blocks_after(self.l2.head_l1_origin);

        // Update the anchor block of the outstanding batch if it's empty.
        self.update_outstanding_batch_anchor_if_needed();

        // Update the preconfirmer lookahead
        let status = self.lookahead.sync(slot).await;
        self.update_status(status);

        Ok(slot)
    }

    /// Trigger the proposal of the local `OutstandingBatch` to L1, and update the anchor block
    /// to the current highest L1 block. These operations MUST always be kept atomic to ensure that
    /// each batch is proposed with its correct anchor block.
    async fn submit_local_outstanding_batch(&mut self, deadline: Slot) {
        // Use the cached anchor block as the new anchor block.
        let new_anchor = self.l1.anchor.clone();

        info!(
            anchor_block_id = self.outstanding_batch.anchor().number,
            blocks = self.outstanding_batch.blocks().len(),
            "Submitting outstanding batch"
        );

        if let Some(block_number) = self.block_builder.state().is_finalizing() {
            let start = Instant::now();
            warn!(block_number, "Waiting for finalization before submitting batch...");
            // Wait for the builder to be idle before submitting the batch
            self.block_builder.state_mut().wait_for_idle().await;
            debug!(block_number, elapsed = ?start.elapsed(), "Finalized block, submitting batch");
        }

        let batch = self.outstanding_batch.take_and_reset(new_anchor);
        let req = BatchSubmissionRequest::new(batch, deadline);
        self.batch_submitter.trigger_batch_submission(req);
        DriverMetrics::set_outstanding_batch_blocks_count(0);
    }

    /// Finalizes all the blocks from the provided batches on the L2 chain.
    ///
    /// NOTE: this may cause an L2 reorg! Use carefully.
    ///
    /// # Recommended usage
    ///
    /// This can be used when a L2 reorg needs to triggered on demand before publishing reorged
    /// blocks in multiple batches, so that we only reorg once.
    async fn finalize_all_blocks_from_batches(
        &self,
        batches: &[Batch],
        reorg_reasons: ReorgReasons,
        basefee_map: HashMap<BlockNumber, u64>,
    ) -> Result<(), DriverError> {
        for batch in batches {
            let anchor = batch.anchor();
            for block in batch.blocks() {
                let parent_bn = block.number().saturating_sub(1);
                let parent = self.l2.el.get_header(Some(parent_bn)).await?;

                // Build a custom payload with the exact transactions and parent block we want
                let mut req = BuildPayloadRequest::new(block.timestamp(), parent)
                    .with_transaction_list(block.tx_list().to_vec())
                    .with_force_empty(true);

                if reorg_reasons.only_reanchoring() {
                    // NOTE(thedevbirb): we can keep the same base fee, calculating a new one
                    // from `TaikoAnchor.getBaseFeeV2` will result in a underflow error.
                    // See: <https://github.com/taikoxyz/taiko-mono/blob/60736f8bbc9af495ef459246af9e6690f4649ea2/packages/protocol/contracts/layer2/based/TaikoAnchor.sol#L307-L321>.
                    if let Some(base_fee) = basefee_map.get(&block.number()).copied() {
                        req = req.with_base_fee(base_fee);
                    } else {
                        let err = format!(
                            "Could not find base fee for block {}. This will cause a pendulum reorg!",
                            block.number()
                        );
                        return Err(DriverError::Custom(err));
                    }
                }

                let payload = self.block_builder.build_payload(req).await?.expect("force_empty");
                self.block_builder.finalize_block(anchor.clone(), payload).await?;
            }
        }

        Ok(())
    }

    /// Updates the driver's status if it has changed and logs the transition.
    fn update_status(&mut self, new_status: DriverStatus) {
        // Short-circuit if the preconf router is not set in the `TaikoWrapper` contract.
        if !self.l1.is_preconf_router_set() {
            warn!("⚠️ PreconfRouter is not set in TaikoWrapper. Setting the driver to INACTIVE");
            self.status = DriverStatus::Inactive;
            return;
        }

        let old_status = self.status;

        if old_status != new_status {
            // When transitioning from `HandoverWait` to `Active`,
            // save the duration of the handover to the metrics.
            if let DriverStatus::HandoverWait { start } = old_status {
                if new_status == DriverStatus::Active {
                    DriverMetrics::set_handover_wait_duration(start.elapsed());
                }
            }

            self.status = new_status;
            DriverMetrics::set_driver_status(new_status);

            debug!(old = ?old_status, new = ?self.status, "🚦 Driver status updated");
        }
    }

    /// Updates the head L1 origin of the driver.
    async fn update_head_l1_origin(&mut self) -> Result<(), DriverError> {
        let protocol_value = self.l1.taiko_inbox.get_head_l1_origin().await?;

        self.l2.head_l1_origin = protocol_value;
        DriverMetrics::set_head_l1_origin(protocol_value);

        // Compare the origin from the inbox contract with the one from taiko-geth
        let l2_el = self.l2.el.clone();
        tokio::spawn(async move {
            if let Ok(Some(node_value)) = l2_el.head_l1_origin_unsafe().await {
                let node_value = node_value.block_id.to::<u64>();
                DriverMetrics::set_node_head_l1_origin(node_value);
                debug!("👽 Head L1 origin: protocol={}, node={}", protocol_value, node_value);
            }
        });

        Ok(())
    }

    /// Updates the anchor block of the outstanding batch.
    ///
    /// This is a no-op if the following conditions are met:
    /// - The batch is not empty
    /// - The builder is finalizing a block, meaning that a block with the previous anchor is still
    ///   being built, and we don't want to change the anchor block in the meantime.
    fn update_outstanding_batch_anchor_if_needed(&mut self) {
        if self.outstanding_batch.is_empty() {
            // If the builder is finalizing a block, don't change the anchor block because
            // that will result in an anchor mismatch (race condition).
            if let Some(block_number) = self.block_builder.state().is_finalizing() {
                warn!(block_number, "L2 block is being finalized, skipping anchor change");
                return;
            }

            self.outstanding_batch.change_anchor(self.l1.anchor.clone());
        }
    }
}
