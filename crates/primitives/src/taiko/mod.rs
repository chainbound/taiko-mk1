use alloy_primitives::{B256, U256};
use serde::{Deserialize, Serialize};

/// Constants for Taiko stack chains.
pub mod constants;

/// Block-building utilities and types.
pub mod builder;

/// Fixed-K signature generation utilities.
pub mod deterministic_signer;

/// Batching utilities and types.
pub mod batch;

/// Batch map type.
pub mod batch_map;

/// Block-related types and utilities.
pub mod block;

/// The `L1Origin` of an L2 block in Taiko.
///
/// Ref: <https://github.com/taikoxyz/taiko-geth/blob/update_preonf_genesis/core/rawdb/taiko_l1_origin.go#L31>
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L1Origin {
    /// L2 block number.
    #[serde(rename = "blockID")]
    pub block_id: U256,
    /// L2 block hash.
    pub l2_block_hash: B256,
    /// L1 block height.
    pub l1_block_height: U256,
    /// L1 block hash.
    pub l1_block_hash: B256,
}
