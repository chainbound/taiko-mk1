use ITaikoInbox::{
    BaseFeeConfig, BatchParams, BatchProposed, ITaikoInboxErrors, ITaikoInboxInstance,
    ProtocolConfig,
};
use alloy::{
    consensus::BlobTransactionSidecar,
    contract::Result as ContractResult,
    rpc::{
        client::ClientBuilder,
        types::{Filter, TransactionReceipt},
    },
    signers::local::PrivateKeySigner,
    sol,
};
use alloy_primitives::{Address, B256, BlockNumber, Bytes};
use alloy_sol_types::{Error as SolError, SolValue};
use derive_more::derive::Deref;
use mk1_primitives::{retries::DEFAULT_RETRY_LAYER, summary::Summary};
use tracing::error;
use url::Url;

use crate::{
    WalletProviderWithSimpleNonceManager, new_wallet_provider_with_simple_nonce_management,
    try_parse_contract_error,
};

/// A wrapper over a `ITaikoInbox` contract that exposes various utility methods.
#[derive(Debug, Clone, Deref)]
pub struct TaikoInbox(ITaikoInboxInstance<WalletProviderWithSimpleNonceManager>);

impl TaikoInbox {
    /// Create a new `TaikoInbox` instance at the given contract address.
    pub fn new<U: Into<Url>>(el_client_url: U, address: Address, wallet: PrivateKeySigner) -> Self {
        let client = ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(el_client_url.into());

        let provider = new_wallet_provider_with_simple_nonce_management(client, wallet);

        Self(ITaikoInboxInstance::new(address, provider))
    }

    /// Propose a batch of blocks with a blob transaction sidecar.
    pub async fn propose_batch(
        &self,
        params: BatchParams,
        tx_list: Bytes,
        sidecar: BlobTransactionSidecar,
    ) -> ContractResult<TransactionReceipt> {
        let encoded_params = Bytes::from(params.abi_encode());

        match self.0.proposeBatch(encoded_params, tx_list).sidecar(sidecar).send().await {
            Ok(pending_res) => Ok(pending_res.get_receipt().await?),
            Err(err) => {
                let decoded_error = try_parse_contract_error::<ITaikoInboxErrors>(err)?;
                Err(SolError::custom(format!("{:?}", decoded_error)).into())
            }
        }
    }

    /// Retrieves the current protocol configuration.
    pub async fn get_pacaya_config(&self) -> ContractResult<ProtocolConfig> {
        match self.0.pacayaConfig().call().await {
            Ok(config) => Ok(config),
            Err(err) => {
                let decoded_error = try_parse_contract_error::<ITaikoInboxErrors>(err)?;
                Err(SolError::custom(format!("{:?}", decoded_error)).into())
            }
        }
    }

    /// Retrieves the parent meta hash of the latest batch.
    pub async fn get_parent_meta_hash(&self) -> ContractResult<B256> {
        // NOTE(nico): we need to do 2 rpc calls here because we need the output of the
        // first call as input for the second one.
        match self.0.getStats2().call().await {
            Ok(stats2) => match self.0.getBatch(stats2.numBatches - 1).call().await {
                Ok(batch) => Ok(batch.metaHash),
                Err(err) => {
                    error!("Failed to call getBatch: {:?}", err);
                    let decoded_error = try_parse_contract_error::<ITaikoInboxErrors>(err)?;
                    Err(SolError::custom(format!("{:?}", decoded_error)).into())
                }
            },
            Err(err) => {
                error!("Failed to call getStats2: {:?}", err);
                let decoded_error = try_parse_contract_error::<ITaikoInboxErrors>(err)?;
                Err(SolError::custom(format!("{:?}", decoded_error)).into())
            }
        }
    }

    /// Retrieves the current L2 head L1 origin by looking at the latest batch state.
    pub async fn get_head_l1_origin(&self) -> ContractResult<BlockNumber> {
        let stats2 = self.0.getStats2().call().await?;
        let batch = self.0.getBatch(stats2.numBatches - 1).call().await?;

        Ok(batch.lastBlockId)
    }

    /// Returns a log [`Filter`] based on the `BatchProposed` event.
    pub fn batch_proposed_filter(&self) -> Filter {
        self.0.BatchProposed_filter().filter
    }
}

impl Summary for BatchProposed {
    fn summary(&self) -> String {
        let last_block_ts = self.info.lastBlockTimestamp;
        let total_shift = self.info.blocks.iter().map(|b| u64::from(b.timeShift)).sum::<u64>();
        let first_block_ts = last_block_ts.saturating_sub(total_shift);
        let number_of_blocks = self.info.blocks.len() as u64;

        format!(
            "batch id: {}, proposer: {}, coinbase: {}, blob hashes: {:?}, proposed in: {}, blocks: {}, first block ts: {}, last block ts: {}, last block id: {}, total shift: {}, anchor block id: {}, anchor block hash: {:?}",
            self.meta.batchId,
            self.meta.proposer,
            self.info.coinbase,
            self.info.blobHashes,
            self.info.proposedIn,
            number_of_blocks,
            first_block_ts,
            last_block_ts,
            self.info.lastBlockId,
            total_shift,
            self.info.anchorBlockId,
            self.info.anchorBlockHash
        )
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    interface ITaikoInbox {
        error AnchorBlockIdSmallerThanParent();
        error AnchorBlockIdTooLarge();
        error AnchorBlockIdTooSmall();
        error ArraySizesMismatch();
        error BatchNotFound();
        error BatchVerified();
        error BeyondCurrentFork();
        error BlobNotFound();
        error BlockNotFound();
        error BlobNotSpecified();
        error ContractPaused();
        error CustomProposerMissing();
        error CustomProposerNotAllowed();
        error EtherNotPaidAsBond();
        error FirstBlockTimeShiftNotZero();
        error ForkNotActivated();
        error InsufficientBond();
        error InvalidBlobCreatedIn();
        error InvalidBlobParams();
        error InvalidGenesisBlockHash();
        error InvalidParams();
        error InvalidTransitionBlockHash();
        error InvalidTransitionParentHash();
        error InvalidTransitionStateRoot();
        error MetaHashMismatch();
        error MsgValueNotZero();
        error NoBlocksToProve();
        error NotFirstProposal();
        error NotInboxWrapper();
        error ParentMetaHashMismatch();
        error SameTransition();
        error SignalNotSent();
        error TimestampSmallerThanParent();
        error TimestampTooLarge();
        error TimestampTooSmall();
        error TooManyBatches();
        error TooManyBlocks();
        error TooManySignals();
        error TransitionNotFound();
        error ZeroAnchorBlockHash();
        error Error(string);

        // These errors are from IPreconfRouter.sol, which extends ITaikoInbox.sol.
        error ForcedInclusionNotSupported();
        error NotFallbackPreconfer();
        error NotPreconfer();
        error ProposerIsNotPreconfer();

        // Error added in <https://github.com/taikoxyz/taiko-mono/pull/19488>
        error InvalidLastBlockId(uint96 _actual, uint96 _expected);

        // This error is from the old implementation of TaikoInbox.sol, sometimes
        // still used for testing on devnets. Better to keep it for redundancy.
        error NotTheOperator();

        // These errors are from TaikoWrapper.sol, which inherits most of ITaikoInbox.sol
        error InvalidBlockTxs();
        error InvalidBlobHashesSize();
        error InvalidBlobHash();
        error InvalidBlobByteOffset();
        error InvalidBlobByteSize();
        error InvalidBlockSize();
        error InvalidTimeShift();
        error InvalidSignalSlots();
        error OldestForcedInclusionDue();

        #[derive(Default)]
        event BatchProposed(BatchInfo info, BatchMetadata meta, bytes txList);

        #[derive(Copy, Default)]
        struct BaseFeeConfig {
            uint8 adjustmentQuotient;
            uint8 sharingPctg;
            uint32 gasIssuancePerSecond;
            uint64 minGasExcess;
            uint32 maxGasIssuancePerBlock;
        }

        #[derive(Default)]
        struct BlockParams {
            // Number of transactions in the block
            uint16 numTransactions;
            // Time shift in seconds
            uint8 timeShift;
            // Signals sent on L1 and need to sync to this L2 block.
            bytes32[] signalSlots;
        }

        /// @dev This struct holds batch information essential for constructing blocks offchain, but it
        /// does not include data necessary for batch proving.
        #[derive(Default)]
        struct BatchInfo {
            bytes32 txsHash;
            // Data to build L2 blocks
            BlockParams[] blocks;
            bytes32[] blobHashes;
            bytes32 extraData;
            address coinbase;
            uint64 proposedIn; // Used by node/client
            uint64 blobCreatedIn;
            uint32 blobByteOffset;
            uint32 blobByteSize;
            uint32 gasLimit;
            uint64 lastBlockId;
            uint64 lastBlockTimestamp;
            // Data for the L2 anchor transaction, shared by all blocks in the batch
            uint64 anchorBlockId;
            // corresponds to the `_anchorStateRoot` parameter in the anchor transaction.
            // The batch's validity proof shall verify the integrity of these two values.
            bytes32 anchorBlockHash;
            BaseFeeConfig baseFeeConfig;
        }

        #[derive(Default)]
        struct BatchMetadata {
            bytes32 infoHash;
            address proposer;
            uint64 batchId;
            uint64 proposedAt; // Used by node/client
        }

        #[derive(Default)]
        struct BlobParams {
            // The hashes of the blob. Note that if this array is not empty.  `firstBlobIndex` and
            // `numBlobs` must be 0.
            bytes32[] blobHashes;
            // The index of the first blob in this batch.
            uint8 firstBlobIndex;
            // The number of blobs in this batch. Blobs are initially concatenated and subsequently
            // decompressed via Zlib.
            uint8 numBlobs;
            // The byte offset of the blob in the batch.
            uint32 byteOffset;
            // The byte size of the blob.
            uint32 byteSize;
            // The block number when the blob was created.
            uint64 createdIn;
        }

        #[derive(Default)]
        struct BatchParams {
            address proposer;
            address coinbase;
            bytes32 parentMetaHash;
            uint64 anchorBlockId;
            uint64 lastBlockTimestamp;
            bool revertIfNotFirstProposal;
            // Specifies the number of blocks to be generated from this batch.
            BlobParams blobParams;
            BlockParams[] blocks;
        }

        #[derive(Default)]
        struct ForkHeights {
            uint64 ontake;
            uint64 pacaya;
        }

        /// @notice Struct holding Taiko configuration parameters. See {TaikoConfig}.
        /// NOTE: this was renamed from "Config" to "ProtocolConfig" for clarity.
        #[derive(Default)]
        struct ProtocolConfig {
            /// @notice The chain ID of the network where Taiko contracts are deployed.
            uint64 chainId;
            /// @notice The maximum number of unverified batches the protocol supports.
            uint64 maxUnverifiedBatches;
            /// @notice Size of the batch ring buffer, allowing extra space for proposals.
            uint64 batchRingBufferSize;
            /// @notice The maximum number of verifications allowed when a batch is proposed or proved.
            uint64 maxBatchesToVerify;
            /// @notice The maximum gas limit allowed for a block.
            uint32 blockMaxGasLimit;
            /// @notice The amount of Taiko token as a prover liveness bond per batch.
            uint96 livenessBondBase;
            /// @notice The amount of Taiko token as a prover liveness bond per block.
            uint96 livenessBondPerBlock;
            /// @notice The number of batches between two L2-to-L1 state root sync.
            uint8 stateRootSyncInternal;
            /// @notice The max differences of the anchor height and the current block number.
            uint64 maxAnchorHeightOffset;
            /// @notice Base fee configuration
            BaseFeeConfig baseFeeConfig;
            /// @notice The proving window in seconds.
            uint16 provingWindow;
            /// @notice The time required for a transition to be used for verifying a batch.
            uint24 cooldownWindow;
            /// @notice The maximum number of signals to be received by TaikoL2.
            uint8 maxSignalsToReceive;
            /// @notice The maximum number of blocks per batch.
            uint16 maxBlocksPerBatch;
            /// @notice Historical heights of the forks.
            ForkHeights forkHeights;
        }

        /// @notice 3 slots used.
        struct Batch {
            bytes32 metaHash; // slot 1
            uint64 lastBlockId; // slot 2
            uint96 reserved3;
            uint96 livenessBond;
            uint64 batchId; // slot 3
            uint64 lastBlockTimestamp;
            uint64 anchorBlockId;
            uint24 nextTransitionId;
            uint8 reserved4;
            // The ID of the transaction that is used to verify this batch. However, if this batch is
            // not verified as the last one in a transaction, verifiedTransitionId will remain zero.
            uint24 verifiedTransitionId;
        }

        struct Stats2 {
            uint64 numBatches;
            uint64 lastVerifiedBatchId;
            bool paused;
            uint56 lastProposedIn;
            uint64 lastUnpausedAt;
        }

        /// @notice Proposes a batch of blocks.
        /// @param _params ABI-encoded BlockParams.
        /// @param _txList The transaction list in calldata. If the txList is empty, blob will be used
        /// for data availability.
        /// @return info_ The info of the proposed batch.
        /// @return meta_ The metadata of the proposed batch.
        function proposeBatch(
            bytes calldata _params,
            bytes calldata _txList
        )
            external
            returns (BatchInfo memory info_, BatchMetadata memory meta_);

        /// @notice Function only callable on PreconfRouter.sol.
        /// @param _params ABI-encoded BlockParams.
        /// @param _txList The transaction list in calldata. If the txList is empty, blob will be used
        /// for data availability.
        /// @param _expectedLastBlockId The expected last block ID.
        /// @return info_ The info of the proposed batch.
        /// @return meta_ The metadata of the proposed batch.
        function proposeBatchWithExpectedLastBlockId(
            bytes calldata _params,
            bytes calldata _txList,
            uint96 _expectedLastBlockId
        ) external returns (BatchInfo memory info_, BatchMetadata memory meta_);

        /// @notice Retrieves the current protocol configuration.
        /// @return The current configuration.
        function pacayaConfig() external view returns (ProtocolConfig memory);

        /// @notice Retrieves the current stats2.
        /// @return The current stats2.
        function getStats2() external view returns (Stats2 memory);

        /// @notice Retrieves a batch by its ID.
        /// @param batchId The ID of the batch to retrieve.
        /// @return The batch.
        function getBatch(uint64 batchId) public view returns (Batch memory);
    }
}

impl BaseFeeConfig {
    /// Converts the `BaseFeeConfig` to its primitive representation.
    pub const fn into_primitive(self) -> mk1_primitives::taiko::builder::BaseFeeConfig {
        mk1_primitives::taiko::builder::BaseFeeConfig {
            adjustment_quotient: self.adjustmentQuotient,
            sharing_pctg: self.sharingPctg,
            gas_issuance_per_second: self.gasIssuancePerSecond,
            min_gas_excess: self.minGasExcess,
            max_gas_issuance_per_block: self.maxGasIssuancePerBlock,
        }
    }

    /// Encodes the block.extraData field from the given _sharing percentage_.
    /// Its minimal big‑endian representation is copied into a 32‑byte array.
    pub fn encoded_extra_data(&self) -> B256 {
        let mut bytes32_value = [0u8; 32];

        // Convert the sharing percentage to u64 and get its big-endian representation.
        let uint_bytes = u64::from(self.sharingPctg).to_be_bytes();

        // Find the first non-zero byte to get the minimal representation.
        let first_nonzero = uint_bytes.iter().position(|&b| b != 0).unwrap_or(uint_bytes.len());
        let minimal = &uint_bytes[first_nonzero..];

        // Copy the minimal bytes into the rightmost part of the 32-byte array.
        let start = 32 - minimal.len();
        bytes32_value[start..].copy_from_slice(minimal);

        bytes32_value.into()
    }
}

impl BatchProposed {
    /// Returns the block numbers that were proposed in this batch, by looking
    /// at the `info.blocks` and `lastBlockId` fields.
    pub fn block_numbers_proposed(&self) -> Vec<u64> {
        let last = self.info.lastBlockId;

        let count = self.info.blocks.len() as u64;

        // Add 1 to avoid off-by-one errors.
        // Example: `last == 3`, `count == 3`, then `first == 1`.
        let first = last.saturating_sub(count) + 1;

        (first..=last).collect()
    }

    /// Returns the last block number proposed in this batch.
    pub const fn last_block_number(&self) -> u64 {
        self.info.lastBlockId
    }

    /// Returns the last block timestamp proposed in this batch.
    pub const fn last_block_timestamp(&self) -> u64 {
        self.info.lastBlockTimestamp
    }
}

impl Summary for BatchParams {
    fn summary(&self) -> String {
        let last_block_ts = self.lastBlockTimestamp;
        let total_shift = self.blocks.iter().map(|b| u64::from(b.timeShift)).sum::<u64>();
        let first_block_ts = last_block_ts.saturating_sub(total_shift);
        let number_of_blocks = self.blocks.len() as u64;

        format!("({} blocks, timestamps: {}..{})", number_of_blocks, first_block_ts, last_block_ts)
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Instant};

    use ITaikoInbox::{BatchInfo, BlockParams};

    use super::*;

    #[test]
    fn test_block_numbers_proposed() {
        let batch = BatchProposed {
            info: BatchInfo {
                lastBlockId: 3,
                blocks: vec![BlockParams::default(); 3],
                ..Default::default()
            },
            ..Default::default()
        };

        let actual = batch.block_numbers_proposed();
        let expected = vec![1, 2, 3];

        assert_eq!(expected, actual, "expected {expected:?}, actual {actual:?}");
    }

    #[ignore]
    #[tokio::test]
    async fn test_get_head_l1_origin_helder() {
        let inbox = TaikoInbox::new(
            Url::parse("https://rpc.helder-devnets.xyz").unwrap(),
            Address::from_str("0xA15933252102952D923197C26F144227097988B9").unwrap(),
            PrivateKeySigner::random(),
        );

        let start = Instant::now();
        let head_l1_origin = inbox.get_head_l1_origin().await.unwrap();
        println!("head_l1_origin: {head_l1_origin}, elapsed: {:?}", start.elapsed());
    }
}
