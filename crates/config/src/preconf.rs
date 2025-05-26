use clap::Parser;

/// Preconfirmation-related configuration options
#[derive(Debug, Clone, Parser)]
pub struct PreconfOpts {
    /// The minimum tip in wei to use for building transaction lists.
    /// Only transactions with this tip or higher will be selected.
    #[clap(long = "preconf.min-tip-wei", env = "MK1_PRECONF_MIN_TIP_WEI", default_value_t = 0)]
    pub min_tip_wei: u64,
    /// The minimum max priority fee per gas that we will use for batch proposals.
    /// This should be set with at least what Geth uses as a default value for the flag
    /// `--miner.gasprice`.
    ///
    /// Reference: <https://github.com/ethereum/go-ethereum/blob/43883c64566602305c079ed6a7c83183f0443dfb/miner/miner.go#L53-L63>
    ///
    /// A related discussion: <https://github.com/ethereum/go-ethereum/pull/28933>
    #[clap(
        long = "preconf.min-batch-tip-wei",
        env = "MK1_PRECONF_MIN_BATCH_TIP_WEI",
        default_value_t = 1_000_000
    )]
    pub min_batch_tip_wei: u128,
    /// Whether to enable building empty blocks at the L2 block heartbeat.
    /// If enabled, we will build a block every [`ChainOpts::l2_block_time`] seconds
    /// even if there are no transactions in the L2 mempool. By default, this is false.
    #[clap(
        long = "preconf.enable-building-empty-blocks",
        env = "MK1_PRECONF_ENABLE_BUILDING_EMPTY_BLOCKS",
        default_value_t = false
    )]
    pub enable_building_empty_blocks: bool,
    /// The slots offset in an epoch to start and end sequencing transactions, so that there is
    /// some room for the sequencer to post the L1 batch with the final blocks.
    ///
    /// Example: suppose we are at slot 16, we will be the sequencer in epoch 2 and the value is
    /// set to `4`. Then we will start sequencing at slot `32 - 4 = 28` included and end at slot
    /// `64 - 4 = 60` excluded.
    /// During those last slots we'll have the time to post the L1 batch with the final blocks.
    #[clap(
        long = "preconf.handover-window-slots",
        env = "MK1_PRECONF_HANDOVER_WINDOW_SLOTS",
        default_value_t = 4
    )]
    pub handover_window_slots: u64,
    /// The number of L2 slots to skip when the sequencing window starts, to increase the chances
    /// we receive the previous' sequencer last block before starting to sequence.
    ///
    /// NOTE: must be less than an L1 slot.
    #[clap(
        long = "preconf.handover-skip-slots",
        env = "MK1_PRECONF_HANDOVER_SKIP_SLOTS",
        default_value_t = 1
    )]
    pub handover_skip_slots: u64,
    /// The number of L1 blocks behind the L1 head to use as the anchor block. This is used to
    /// minimize L2 reorg risk.
    ///
    /// This is only a guideline. If a previously sequenced L2 block has an anchor that would be
    /// higher than this block, we will adopt that anchor block instead.
    #[clap(
        long = "preconf.anchor-block-lag",
        env = "MK1_PRECONF_ANCHOR_BLOCK_LAG",
        default_value_t = 4
    )]
    pub anchor_block_lag: u64,
    /// The buffer to account for the maximum anchor block height used in a batch transaction.
    ///
    /// Rationale: when the anchor block height for an outstanding batch is getting close to the
    /// configured limit, we rapidly need to propose a batch to L1 to ensure that it will be
    /// accepted. We use this buffer to ensure we have enough time to propose before the limit
    /// is hit.
    ///
    /// Failure to include the batch after the `pacaya_config.maxAnchorHeightOffset` will result in
    /// the batch transaction reverting with `AnchorBlockIdTooSmall()` error.
    #[clap(
        long = "preconf.anchor-max-height-buffer",
        env = "MK1_PRECONF_ANCHOR_MAX_HEIGHT_BUFFER",
        default_value_t = 6
    )]
    pub anchor_max_height_buffer: u64,
    /// The target (compressed) size of a batch in kilobytes. Note that we could go under or over
    /// this value. This should be used as a rough target, and not a strict limit.
    ///
    /// ### Default value
    ///
    /// One blob can hold 130044b of data. Targeting 3 blobs = 390132b / 1024 = 380kb
    #[clap(
        long = "preconf.batch-size-target-kb",
        env = "MK1_PRECONF_BATCH_SIZE_TARGET_KB",
        default_value_t = 380
    )]
    pub batch_size_target_kb: u64,

    /// The maximum size of an L2 block's transaction list,  in bytes.
    ///
    /// This value might be scaled down if we are getting throttled on L1 DA.
    #[clap(
        long = "preconf.max-block-size-bytes",
        env = "MK1_PRECONF_MAX_BLOCK_SIZE_BYTES",
        default_value_t = 126_976
    )]
    pub max_block_size_bytes: u64,
    /// The DA throttling factor.
    ///
    /// This is used to scale down the maximum block size if we are getting throttled on L1 DA.
    ///
    /// ### Default value
    ///
    /// 12, which means that we scale down the maximum block size by 1/12 for each batch in queue.
    #[clap(
        long = "preconf.da-throttling-factor",
        env = "MK1_PRECONF_DA_THROTTLING_FACTOR",
        default_value_t = 12
    )]
    pub da_throttling_factor: u64,
    /// Whether to disable the expected last block ID feature.
    ///
    /// This feature is only available on `PreconfRouter.sol` after
    /// <https://github.com/taikoxyz/taiko-mono/pull/19488>.
    #[clap(
        long = "preconf.disable-expected-last-block-id",
        env = "MK1_PRECONF_DISABLE_EXPECTED_LAST_BLOCK_ID",
        default_value_t = false
    )]
    pub disable_expected_last_block_id: bool,
    /// The maximum number of transaction lists fetched by the L2 engine.
    #[clap(
        long = "preconf.max-tx-lists-per-call",
        env = "MK1_PRECONF_MAX_TX_LISTS_PER_CALL",
        default_value_t = 1
    )]
    pub max_tx_lists_per_call: u64,
}
