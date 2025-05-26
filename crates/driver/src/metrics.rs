use std::time::Duration;

use alloy::rpc::types::TransactionReceipt;
use alloy_primitives::Address;
use metrics::{counter, gauge, histogram};

use crate::status::DriverStatus;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DriverMetrics;

impl DriverMetrics {
    // ################ COUNTERS ################ //

    // ============= L1 / L2 STATE ================ //

    /// Sets the L1 beacon chain head (aka: latest slot number)
    pub(crate) fn set_l1_head_slot(value: u64) {
        counter!("driver_l1_head_slot").absolute(value);
    }

    /// Sets the L1 execution client head (aka: latest block number)
    pub(crate) fn set_l1_head_number(value: u64) {
        counter!("driver_l1_head_number").absolute(value);
    }

    /// Sets the L1 anchor block number.
    pub(crate) fn set_l1_anchor_block(id: u64) {
        counter!("driver_l1_anchor_block").absolute(id);
    }

    /// Sets the L2 execution client head (aka: latest block number)
    pub(crate) fn set_l2_el_head(value: u64) {
        counter!("driver_l2_el_head").absolute(value);
    }

    /// Sets the head L1 origin of the L2 execution client.
    pub(crate) fn set_head_l1_origin(value: u64) {
        counter!("driver_head_l1_origin").absolute(value);
    }

    /// Sets the head L1 origin of the L2 execution client.
    pub(crate) fn set_node_head_l1_origin(value: u64) {
        counter!("driver_node_head_l1_origin").absolute(value);
    }

    /// Increments the amount of L2 reorgs detected.
    pub(crate) fn increment_l2_reorgs(new_head: u64, old_head: u64) {
        counter!("driver_l2_reorgs", "old_head" => old_head.to_string(), "new_head" => new_head.to_string()).increment(1);
    }

    // ============= Batches ================ //

    /// Increments the amount of batches reanchored
    pub(crate) fn increment_batches_reanchored(count: u64, old_anchor: u64, new_anchor: u64) {
        counter!(
            "driver_batches_reanchored",
            "old_anchor" => old_anchor.to_string(),
            "new_anchor" => new_anchor.to_string()
        )
        .increment(count);
    }

    /// Increments the amount of batches retimestamped
    pub(crate) fn increment_batches_retimestamped(new_ts: u64) {
        counter!(
            "driver_batches_retimestamped",
            "new_timestamp" => new_ts.to_string()
        )
        .increment(1);
    }

    /// Increments the amount of L1 batches submitted by the driver
    pub(crate) fn increment_total_l1_batches_submitted() {
        counter!("driver_total_l1_batches_submitted").increment(1);
    }

    /// Increments the amount of L1 batches successfully included by the driver
    pub(crate) fn increment_l1_batches_included() {
        counter!("driver_l1_batches_included").increment(1);
    }

    /// Increments the amount of L1 batches included but reverted by the driver
    pub(crate) fn increment_l1_batches_reverted(reason: String) {
        counter!("driver_l1_batches_reverted", "reason" => reason).increment(1);
    }

    /// Increments the amount of L1 batches failed to create by reason
    pub(crate) fn increment_batch_creation_failures(reason: String) {
        counter!("driver_batch_creation_failures", "reason" => reason).increment(1);
    }

    /// Increments the amount of L1 batch submission failures by reason
    pub(crate) fn increment_batch_submission_failures(reason: String) {
        counter!("driver_batch_submission_failures", "reason" => reason).increment(1);
    }

    /// Increments the amount of pending L1 batch submission dropped due to a failure
    pub(crate) fn increment_batch_submissions_dropped(reason: String, value: usize) {
        counter!("driver_batch_submissions_dropped", "reason" => reason).increment(value as u64);
    }

    /// Increments the amount of batches submitted
    pub(crate) fn increment_unsafe_batches_submitted(info: String) {
        counter!("driver_unsafe_batches_submitted", "info" => info).increment(1);
    }

    // ============ L2 BLOCKS ================ //

    /// Increments the amount of L2 blocks built by the driver
    pub(crate) fn increment_l2_blocks_built() {
        counter!("driver_l2_blocks_built").increment(1);
    }

    /// Increments the amount of block production failures by reason
    pub(crate) fn increment_block_production_failures(reason: String) {
        counter!("driver_block_production_failures", "reason" => reason).increment(1);
    }

    /// Increments the amount of blocks skipped due to finalization lag
    pub(crate) fn increment_skipped_blocks_due_to_finalization_lag(block_number: u64) {
        counter!("driver_skipped_blocks_due_to_finalization_lag", "block_number" => block_number.to_string())
            .increment(1);
    }

    /// Increments the amount of blocks skipped due to parent block too low
    pub(crate) fn increment_skipped_blocks_due_to_parent_block_too_low(
        parent_block_number: u64,
        highest_unsafe_block: u64,
    ) {
        counter!(
            "driver_skipped_blocks_due_to_parent_block_too_low",
            "parent_block_number" => parent_block_number.to_string(),
            "highest_unsafe_block" => highest_unsafe_block.to_string()
        )
        .increment(1);
    }

    /// Increments the amount of batch block gaps detected
    pub(crate) fn increment_batch_block_gaps(depth: u64) {
        counter!("driver_batch_block_gaps", "depth" => depth.to_string()).increment(1);
    }

    /// Increments the amount of batch block gaps fill failures
    pub(crate) fn increment_batch_block_gaps_fill_failures(reason: String) {
        counter!(
            "driver_batch_block_gaps_fill_failures",
            "reason" => reason
        )
        .increment(1);
    }

    // ################ GAUGES ################ //

    /// Sets the version of the MK1 driver.
    pub(crate) fn set_mk1_version(tag: String) {
        gauge!("driver_mk1_version", "tag" => tag).set(1.0);
    }

    // ============ Blocks ================ //

    /// Sets the amount of soft blocks downloaded
    pub(crate) fn set_soft_blocks_count(count: u64) {
        gauge!("driver_soft_blocks_count").set(count as f64);
    }

    // ============ Batches ================ //

    /// Sets the amount of blobs packed in an L1 batch
    pub(crate) fn set_batch_blob_count(count: usize) {
        gauge!("driver_batch_blob_count").set(count as f64);
    }

    /// Sets the size of an L1 batch in bytes (before and after compression).
    /// This is a running cursor when the batch is being built, not necessarily the final size.
    pub(crate) fn set_batch_size(raw: usize, compressed: u32) {
        gauge!("driver_batch_raw_size_bytes").set(raw as f64);
        gauge!("driver_batch_compressed_size_bytes").set(f64::from(compressed));
        gauge!("driver_batch_compression_ratio").set(f64::from(compressed) / raw as f64);
    }

    /// Sets the amount of blobspace wasted in an L1 batch
    pub(crate) fn set_batch_blobspace_wasted(wasted: usize) {
        gauge!("driver_batch_blobspace_wasted").set(wasted as f64);
    }

    /// Sets the size of an L1 batch in bytes when it is actually proposed
    /// (before and after compression)
    pub(crate) fn set_proposed_batch_size(raw: usize, compressed: u32) {
        gauge!("driver_proposed_batch_raw_size_bytes").set(raw as f64);
        gauge!("driver_proposed_batch_compressed_size_bytes").set(f64::from(compressed));
        gauge!("driver_proposed_batch_compression_ratio").set(f64::from(compressed) / raw as f64);
    }

    /// Sets the gas cost of an L1 batch in wei (gas + blob gas)
    pub(crate) fn set_batch_gas_cost_in_wei(receipt: &TransactionReceipt) {
        let gas_cost = receipt.gas_used + receipt.effective_gas_price as u64;
        let blob_cost =
            receipt.blob_gas_price.unwrap_or(0) as u64 + receipt.blob_gas_used.unwrap_or(0);

        let gas_cost_in_wei = gas_cost + blob_cost;

        gauge!("driver_batch_gas_cost_in_wei").set(gas_cost_in_wei as f64);
    }

    /// Sets the amount of attempts to build an L1 batch
    pub(crate) fn set_batch_attempts(attempts: u64) {
        gauge!("driver_batch_attempts").set(attempts as f64);
    }

    // =========== L1 state ================== //

    /// Sets the amount of slots left in the L1 epoch.
    pub(crate) fn set_slots_left_in_epoch(value: u64) {
        gauge!("driver_slots_left_in_epoch").set(value as f64);
    }

    // =========== Sequencing ================ //

    /// Sets the age of the L2 anchor block, in slots
    pub(crate) fn set_l2_anchor_block_age(value: u64) {
        gauge!("driver_l2_anchor_block_age").set(value as f64);
    }

    /// Sets the amount of sequencing slots left in the current sequencing window
    pub(crate) fn set_sequencing_slots_left_in_window(value: u64) {
        gauge!("driver_sequencing_slots_left_in_window").set(value as f64);
    }

    /// Sets the sequencing status of the driver.
    pub(crate) fn set_driver_status(status: DriverStatus) {
        let displayed = format!("{status}");

        for other in DriverStatus::variant_names() {
            let other = other.to_string();
            if other == displayed {
                // Don't temporarily set the current status to 0.
                continue;
            }
            gauge!("driver_status", "status" => other).set(0);
        }

        gauge!("driver_status", "status" => displayed).set(1);
    }

    /// Sets the duration of the handover wait period.
    pub(crate) fn set_handover_wait_duration(duration: Duration) {
        gauge!("driver_handover_wait_duration").set(duration.as_secs_f64());
    }

    /// Sets the current operator address of the lookahead.
    pub(crate) fn set_current_operator(address: Address, slot: u64) {
        gauge!("driver_current_operator", "address" => address.to_string()).set(slot as f64);
    }

    /// Sets the operator address at a given epoch.
    pub(crate) fn set_operator_at_epoch(operator: Address, epoch: u64) {
        gauge!("driver_operator_at_epoch", "operator" => operator.to_string()).set(epoch as f64);
    }

    /// Sets the next operator address of the lookahead.
    pub(crate) fn set_next_operator(address: Address, slot: u64) {
        gauge!("driver_next_operator", "address" => address.to_string()).set(slot as f64);
    }

    /// Sets the amount of transactions included in an L2 block
    pub(crate) fn set_block_tx_count(value: u64, internal: bool) {
        gauge!("driver_block_tx_count_gauge", "internal" => internal.to_string()).set(value as f64);
    }

    /// Sets the amount of gas used in an L2 block
    pub(crate) fn set_block_gas_used(value: u64, internal: bool) {
        gauge!("driver_block_gas_used_gauge", "internal" => internal.to_string()).set(value as f64);
    }

    /// Sets the suggested maximum size of an L2 block
    pub(crate) fn set_suggested_max_block_size(size: u64) {
        gauge!("driver_suggested_max_block_size").set(size as f64);
    }

    // ============ Batches ================ //

    /// Sets the size of the batch submission pending queue
    pub(crate) fn set_batch_submission_queue_size(value: usize) {
        gauge!("driver_batch_submission_queue_size").set(value as f64);
    }

    /// Sets the amount of blocks in the driver outstanding batch
    pub(crate) fn set_outstanding_batch_blocks_count(value: usize) {
        gauge!("driver_outstanding_batch_blocks_count").set(value as f64);
    }

    /// Sets the sequencer balance in ETH
    pub(crate) fn set_batch_submitter_balance(value: f64) {
        gauge!("driver_batch_submitter_balance_eth").set(value);
    }

    /// Sets the sequencer balance in TKO
    pub(crate) fn set_batch_submitter_tko_balance(value: f64) {
        gauge!("driver_batch_submitter_balance_tko").set(value);
    }

    // ################ HISTOGRAMS ################ //

    // ============ Blocks ================ //

    /// Records the time it took to build the preconf block on the preconf client
    pub(crate) fn record_build_preconf_block_time(time_elapsed: Duration) {
        histogram!("driver_build_preconf_block_time").record(time_elapsed.as_secs_f64());
    }

    /// Records the time it took to build an L2 block
    pub(crate) fn record_total_block_building_time(time_elapsed: Duration) {
        histogram!("driver_total_block_building_time").record(time_elapsed.as_secs_f64());
    }

    /// Records the time it took to download the soft blocks
    pub(crate) fn record_soft_blocks_download_time(time_elapsed: Duration) {
        histogram!("driver_soft_blocks_download_time").record(time_elapsed.as_secs_f64());
    }

    // ============ Batches ================ //

    /// Records the time it took to include an L1 batch transaction in an L1 block
    pub(crate) fn record_batch_tx_inclusion_time(time_elapsed: Duration) {
        histogram!("driver_batch_tx_inclusion_time").record(time_elapsed.as_secs_f64());
    }

    /// Records the time it took to build an L1 batch
    pub(crate) fn record_batch_building_time(time_elapsed: Duration) {
        histogram!("driver_batch_building_time").record(time_elapsed.as_secs_f64());
    }

    /// Records the time it took to compress an L1 batch
    pub(crate) fn record_batch_compression_time(time_elapsed: Duration) {
        histogram!("driver_batch_compression_time").record(time_elapsed.as_secs_f64());
    }

    // ============ Transactions ================ //

    /// Records the time it took to fetch the transaction list from the L2 engine
    pub(crate) fn record_fetch_txs_time(time_elapsed: Duration) {
        histogram!("driver_fetch_txs_time").record(time_elapsed.as_secs_f64());
    }
}
