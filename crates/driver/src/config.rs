use std::time::Duration;

use alloy::eips::eip7691::MAX_BLOBS_PER_BLOCK_ELECTRA;
use derive_more::derive::{Deref, DerefMut};
use mk1_chainio::taiko::inbox::{ITaikoInbox::ProtocolConfig, TaikoInbox};
use mk1_clients::beacon::BeaconClient;
use mk1_config::Opts;
use mk1_primitives::{
    BYTES_PER_KB, Slot,
    blob::MAX_BLOB_DATA_SIZE,
    summary::Summary,
    taiko::batch::BatchSettings,
    time::{SlotClock, current_timestamp_seconds},
    wei_to_eth,
};
use thiserror::Error;
use tokio::time::Instant;

/// An error that occurs when creating a [`RuntimeConfig`].
#[derive(Debug, Error)]
pub enum RuntimeConfigError {
    #[error(transparent)]
    BeaconApi(#[from] beacon_api_client::Error),
    #[error("Contract error: {0}")]
    Contract(#[from] alloy::contract::Error),
}

/// The runtime configuration for the Mk1 driver.
#[derive(Debug, Clone, Deref, DerefMut)]
pub(crate) struct RuntimeConfig {
    /// CLI options that can be accessed as dereferenced fields.
    #[deref]
    #[deref_mut]
    pub opts: Opts,
    /// The L1 beacon chain genesis timestamp
    pub l1_genesis_timestamp: u64,
    /// Pacaya Protocol config, fetched from the `TaikoInbox` L1 contract.
    pub pacaya_config: ProtocolConfig,
}

impl RuntimeConfig {
    /// Create a new [`RuntimeConfig`] instance from the given [`Opts`].
    pub(crate) async fn from_opts(opts: Opts) -> Result<Self, RuntimeConfigError> {
        let l1_cl = BeaconClient::new(opts.l1.cl_url.clone());
        let l1_genesis_timestamp = l1_cl.get_genesis_details().await?.genesis_time;

        let l1_taiko_inbox = TaikoInbox::new(
            opts.l1.el_url.clone(),
            opts.contracts.taiko_inbox,
            opts.operator.private_key.clone(),
        );

        let pacaya_config = l1_taiko_inbox.get_pacaya_config().await?;

        Ok(Self { opts, l1_genesis_timestamp, pacaya_config })
    }

    /// Returns the current L1 slot.
    pub(crate) fn current_slot(&self) -> Slot {
        mk1_primitives::time::timestamp_to_l1_slot(
            current_timestamp_seconds(),
            self.l1_genesis_timestamp,
            self.chain.l1_block_time,
        )
    }

    /// Helper function to convert a timestamp to a L1 slot.
    pub(crate) const fn timestamp_to_l1_slot(&self, timestamp: u64) -> Slot {
        mk1_primitives::time::timestamp_to_l1_slot(
            timestamp,
            self.l1_genesis_timestamp,
            self.opts.chain.l1_block_time,
        )
    }

    /// Helper function to convert a L1 slot to a timestamp.
    pub(crate) const fn slot_to_timestamp(&self, slot: u64) -> u64 {
        mk1_primitives::time::slot_to_timestamp(
            slot,
            self.l1_genesis_timestamp,
            self.opts.chain.l1_block_time,
        )
    }

    /// Returns the maximum allowed time shift in seconds. If a block's timestamp
    /// is older than this value, its onchain proposal will fail.
    ///
    /// Reference: <https://github.com/taikoxyz/taiko-mono/blob/ed9496991cf4455d15cdd096d4a494f14e31ed01/packages/protocol/contracts/layer1/based/TaikoInbox.sol#L808-L812>
    pub(crate) const fn max_proposable_block_age_secs(&self) -> u64 {
        self.pacaya_config.maxAnchorHeightOffset * self.opts.chain.l1_block_time
    }

    /// The default proposal time buffer to account for the maximum anchor block height used in a
    /// batch transaction. This is a sensible default to make sure that the batch will be
    /// accepted even with the default amount of retries applied.
    pub(crate) const fn default_proposal_time_buffer(&self) -> u64 {
        self.opts.preconf.anchor_max_height_buffer * self.opts.chain.l1_block_time
    }

    /// Returns the default settings for a batch.
    pub(crate) const fn batch_settings(&self) -> BatchSettings {
        BatchSettings {
            capacity: self.pacaya_config.maxBlocksPerBatch as usize,
            target_compressed_size: self.opts.preconf.batch_size_target_kb as usize * BYTES_PER_KB,
            max_raw_size: MAX_BLOB_DATA_SIZE * MAX_BLOBS_PER_BLOCK_ELECTRA as usize,
            default_coinbase: self.opts.operator.private_key.address(),
            max_anchor_height_offset: self.pacaya_config.maxAnchorHeightOffset,
            anchor_max_height_buffer: self.opts.preconf.anchor_max_height_buffer,
        }
    }

    /// Creates a new [`SlotClock`] that starts ticking at the next L1 slot.
    pub(crate) fn slot_clock(&self) -> SlotClock {
        let now = current_timestamp_seconds();

        // The timestamp of the next L1 slot.
        let next_slot_ts = self.l1_genesis_timestamp +
            (self.timestamp_to_l1_slot(now) * self.chain.l1_block_time) +
            self.chain.l1_block_time;

        let duration_until_next = Duration::from_secs(next_slot_ts.saturating_sub(now));

        SlotClock::new_at(
            Instant::now()
                .checked_add(duration_until_next)
                .expect("Time overflow when calculating next block instant"),
            Duration::from_secs(self.chain.l1_block_time),
            Duration::from_secs(self.chain.l2_block_time),
        )
    }
}

impl Summary for RuntimeConfig {
    fn summary(&self) -> String {
        format!(
            "Running with the following configuration:
            - Instance name: {}
            - L1 chain: genesis_timestamp={}, block_time={}s
            - L2 chain: id={}, block_time={}s
            - Operator: address={}, min_eth_balance={:.2}, min_taiko_balance={:.2}
            - Preconf: max_anchor_offset={}, block_gas_limit={}, proving_window={}min,
            - Batch: target_compressed_size={}b, max_raw_size={}b, max_blocks={}
            - Min tip wei: {}, disable_expected_last_block_id={}
            ",
            self.opts.instance_name,
            self.l1_genesis_timestamp,
            self.chain.l1_block_time,
            self.pacaya_config.chainId,
            self.chain.l2_block_time,
            self.opts.operator.private_key.address(),
            wei_to_eth(self.opts.operator.min_eth_balance),
            wei_to_eth(self.opts.operator.min_taiko_balance),
            self.pacaya_config.maxAnchorHeightOffset,
            self.pacaya_config.blockMaxGasLimit,
            self.pacaya_config.provingWindow / 60,
            self.batch_settings().target_compressed_size,
            self.batch_settings().max_raw_size,
            self.batch_settings().capacity,
            self.opts.preconf.min_tip_wei,
            self.opts.preconf.disable_expected_last_block_id,
        )
    }
}
