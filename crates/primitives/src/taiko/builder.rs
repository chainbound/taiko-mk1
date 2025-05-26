use std::fmt;

use alloy::{consensus::TxEnvelope, rpc::types::Header};
use alloy_primitives::{Address, B256, BlockNumber, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Epoch, serialize_address_to_string};

/// [`PreBuiltTxList`] is a pre-built transaction list based on the latest chain state,
/// with estimated gas used / bytes. Each tx list correspond to a Taiko L2 block built by
/// taiko-geth.
///
/// Reference: <https://github.com/taikoxyz/taiko-geth/blob/2448fb97a8b873c7bd7c0051cd83aaea339050e0/miner/taiko_miner.go#L11-L17>
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct PreBuiltTxList {
    /// The list of transactions in the L2 block.
    pub tx_list: Vec<TxEnvelope>,
    /// The estimated gas used in the L2 block.
    pub estimated_gas_used: u64,
    /// The estimated bytes length of the L2 block.
    pub bytes_length: u64,
}

impl fmt::Display for PreBuiltTxList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "count={}, gas={}, bytes={}",
            self.tx_list.len(),
            self.estimated_gas_used,
            self.bytes_length
        )
    }
}

/// The parameters needed for making a call to the endpoint [`TX_POOL_CONTENT_WITH_MIN_TIP`].
///
/// Reference: <https://github.com/taikoxyz/taiko-mono/blob/c047077e6cace1505dd150c2717206b397ce58a8/packages/taiko-client/pkg/rpc/engine.go#L118-L129>
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolContentParams {
    /// The beneficiary address.
    pub beneficiary: Address,
    /// The base fee.
    pub base_fee: u64,
    /// The maximum gas limit for the block.
    pub block_max_gas_limit: u64,
    /// The maximum bytes per transaction list.
    pub max_bytes_per_tx_list: u64,
    /// A list of accounts that are considerd "local". That means the EL
    /// client will prioritize them as those transactions come from the node itself.
    pub local_accounts: Vec<Address>,
    /// The maximum number of transaction lists per call.
    pub max_tx_lists_per_call: u64,
    /// The minimum tip.
    pub min_tip: u64,
}

impl TxPoolContentParams {
    /// Converts the parameters into a vector of JSON values suitable for an RPC call.
    ///
    /// example: ["0x1234", "0x5678", ...]
    pub fn into_rpc_params(self) -> Vec<Value> {
        vec![
            serde_json::to_value(self.beneficiary).expect("infallible"),
            serde_json::to_value(self.base_fee).expect("infallible"),
            serde_json::to_value(self.block_max_gas_limit).expect("infallible"),
            serde_json::to_value(self.max_bytes_per_tx_list).expect("infallible"),
            serde_json::to_value(self.local_accounts).expect("infallible"),
            serde_json::to_value(self.max_tx_lists_per_call).expect("infallible"),
            serde_json::to_value(self.min_tip).expect("infallible"),
        ]
    }
}

/// A request body for building a preconf block.
///
/// Reference: <https://github.com/taikoxyz/taiko-mono/blob/main/packages/taiko-client/driver/preconf_blocks/api.go#L45>
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildPreconfBlockRequestBody {
    /// The data necessary to execute an EL payload.
    pub executable_data: ExecutableData,
    /// If true, the new block will relay the `EndOfSequencing` mark
    #[serde(default)]
    pub end_of_sequencing: bool,
}

/// Data necessary to build an execution payload
#[derive(Debug, Clone, Serialize, Deserialize, RlpDecodable, RlpEncodable)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableData {
    /// Parent hash.
    pub parent_hash: B256,
    /// Fee recipient.
    #[serde(serialize_with = "serialize_address_to_string")]
    pub fee_recipient: Address,
    /// Block number.
    pub block_number: BlockNumber,
    /// Gas limit.
    pub gas_limit: u64,
    /// Timestamp.
    pub timestamp: u64,
    /// The encoded and compressed transaction data. Encoding steps:
    /// 1. Encode the transaction list as RLP
    /// 2. Compress the encoded data using zlib
    pub transactions: Bytes,
    /// Extra data of the payload.
    pub extra_data: Bytes,
    /// Base fee per gas.
    pub base_fee_per_gas: u64,
}

/// Base fee configuration, as defined in the Taiko Pacaya contract bindings.
#[derive(Debug, Clone, Serialize, Deserialize, RlpDecodable, RlpEncodable)]
#[serde(rename_all = "camelCase")]
pub struct BaseFeeConfig {
    /// Adjustment quotient.
    pub adjustment_quotient: u8,
    /// Sharing percentage.
    pub sharing_pctg: u8,
    /// Gas issuance per second.
    pub gas_issuance_per_second: u32,
    /// Minimum gas excess.
    pub min_gas_excess: u64,
    /// Maximum gas issuance per block.
    pub max_gas_issuance_per_block: u32,
}

/// A response body for building a preconf block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BuildPreconfBlockResponseBody {
    /// A successful response.
    Success {
        /// The preconfirmed block header.
        #[serde(rename = "blockHeader")]
        block_header: Header,
    },
    /// An error response.
    Error {
        /// The error message.
        error: String,
    },
}

/// The execution payload for a new block, not yet finalized.
#[derive(Debug)]
pub struct BlockPayload {
    /// The parent block.
    pub parent: Header,
    /// The base fee.
    pub base_fee: u64,
    /// The timestamp.
    pub timestamp: u64,
    /// If true, the new block will relay the `EndOfSequencing` mark
    /// to the Taiko preconf client.
    pub is_last: bool,
    /// The transaction list without the anchor transaction.
    pub tx_list_without_anchor_tx: Vec<TxEnvelope>,
}

/// The status of the Taiko preconf block server.
///
/// Ref: <https://github.com/taikoxyz/taiko-mono/blob/e0e87c25105e871b0a07e5dbfdf952ef037b8b8b/packages/taiko-client/driver/preconf_blocks/api.go#L288>
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaikoPreconfStatus {
    /// The current lookahead information
    pub lookahead: Value,
    /// The total number of cached payloads after the start of the server.
    pub total_cached: u64,
    /// The highest preconfirmation block ID that the server received from
    /// the P2P network. If it's zero, it means that the current server has
    /// not received any preconfirmation block from the P2P network yet.
    #[serde(rename = "highestUnsafeL2PayloadBlockID")]
    pub highest_unsafe_l2_payload_block_id: u64,
    /// The block hash of the end of sequencing block.
    pub end_of_sequencing_block_hash: B256,
}

/// The types of events that can be received from the Taiko preconf Websocket server.
///
/// TODO: add ref once <https://github.com/taikoxyz/taiko-mono/pull/19361> is merged
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub enum TaikoPreconfWsEvent {
    /// A message indicating the end of sequencing for a given epoch.
    EndOfSequencing(EndOfSequencingMessage),
}

/// The message sent by the Taiko preconf block server when the end of sequencing is reached.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndOfSequencingMessage {
    /// The current epoch.
    pub current_epoch: Epoch,
    /// Always true for this type of event.
    /// (Do not remove; used to identify this event type)
    #[serde(default)]
    pub end_of_sequencing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_end_of_sequencing_message() {
        let ser = r#"{"currentEpoch":1,"endOfSequencing":true}"#;
        let event: TaikoPreconfWsEvent = serde_json::from_str(ser).unwrap();
        assert!(matches!(event, TaikoPreconfWsEvent::EndOfSequencing(_)));
    }
}
