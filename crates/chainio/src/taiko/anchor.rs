use ITaikoAnchor::{BaseFeeConfig, ITaikoAnchorInstance, anchorV3Call};
use alloy::{
    consensus::{SignableTransaction, Transaction, TxEnvelope, TypedTransaction},
    contract::Result as ContractResult,
    network::TransactionBuilder,
    providers::Provider,
    rpc::{
        client::ClientBuilder,
        types::{Block, TransactionInput, TransactionRequest},
    },
    signers::local::PrivateKeySigner,
    transports::{TransportError, TransportErrorKind, TransportResult},
};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_macro::sol;
use alloy_sol_types::{Error as SolError, Result as SolResult, SolCall};
use mk1_primitives::{
    retries::DEFAULT_RETRY_LAYER,
    taiko::{
        constants::{
            TAIKO_ANCHOR_V3_GAS_LIMIT, TAIKO_GOLDEN_TOUCH_ACCOUNT, TAIKO_GOLDEN_TOUCH_PRIVATE_KEY,
        },
        deterministic_signer::sign_hash_deterministic,
    },
};
use url::Url;

use crate::{
    WalletProviderWithSimpleNonceManager, new_wallet_provider_with_simple_nonce_management,
};

/// Errors that can occur when interacting with the Taiko anchor contract
/// or any of its related ABI utilities.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum TaikoAnchorError {
    #[error("anchor transaction not found in block.")]
    AnchorTransactionNotFound,
    #[error("anchor transaction from is not the golden touch account: {got}, expected: {expected}")]
    InvalidSigner { got: Address, expected: Address },
    #[error("failed to decode transaction data: {0}")]
    TransactionDataDecodeError(#[from] SolError),
}

/// A wrapper over an L2 `TaikoAnchor.sol` contract that exposes various utility methods.
/// It uses the default [`TAIKO_GOLDEN_TOUCH_ACCOUNT`] account as signer.
#[derive(Debug, Clone)]
pub struct TaikoAnchor {
    chain_id: u64,
    inner: ITaikoAnchorInstance<WalletProviderWithSimpleNonceManager>,
}

impl TaikoAnchor {
    /// Create a new `TaikoAnchor` instance at the given contract address.
    ///
    /// It uses the default [`TAIKO_GOLDEN_TOUCH_ACCOUNT`] account as signer.
    pub fn new<U: Into<Url>>(el_client_url: U, chain_id: u64, address: Address) -> Self {
        let golden_touch = PrivateKeySigner::from_bytes(&TAIKO_GOLDEN_TOUCH_PRIVATE_KEY).unwrap();
        let client = ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(el_client_url.into());

        let provider = new_wallet_provider_with_simple_nonce_management(client, golden_touch);

        Self { chain_id, inner: ITaikoAnchorInstance::new(address, provider) }
    }

    /// Calculates the base fee and gas excess using EIP-1559 configuration for the given
    /// parameters. Returns the base fee, new gas target, and new gas excess.
    pub async fn get_base_fee_v2(
        &self,
        parent_gas_used: u32,
        block_timestamp: u64,
        base_fee_config: BaseFeeConfig,
    ) -> ContractResult<BaseFeeResult> {
        let data = self
            .inner
            .getBasefeeV2(parent_gas_used, block_timestamp, base_fee_config)
            .call()
            .await?;

        Ok(BaseFeeResult {
            base_fee: data.basefee_,
            new_gas_target: data.newGasTarget_,
            new_gas_excess: data.newGasExcess_,
        })
    }

    /// Anchors the latest L1 block details to L2 for cross-layer message verification.
    ///
    /// This transaction is signed with the `GOLDEN TOUCH` account private key.
    pub async fn build_anchor_v3_tx(
        &self,
        input: TaikoAnchorTxInput,
        parent_hash: B256,
        l2_base_fee: u128,
    ) -> TransportResult<TxEnvelope> {
        let nonce = self
            .inner
            .provider()
            .get_transaction_count(TAIKO_GOLDEN_TOUCH_ACCOUNT)
            .block_id(parent_hash.into())
            .await?;

        let calldata = TransactionInput::new(anchorV3Call::from(input).abi_encode().into());

        // Build the anchor transaction request statically
        let typed_transaction = TransactionRequest::default()
            .from(TAIKO_GOLDEN_TOUCH_ACCOUNT)
            .gas_limit(TAIKO_ANCHOR_V3_GAS_LIMIT)
            .input(calldata)
            .with_chain_id(self.chain_id)
            .nonce(nonce)
            .to(*self.inner.address())
            .max_fee_per_gas(l2_base_fee)
            .transaction_type(2)
            .max_priority_fee_per_gas(0)
            .build_unsigned()
            .expect("missing keys in tx request");

        // By this point, the tx request is fully populated (except for the signature).
        // It is guaranteed to be an EIP-1559 transaction.
        let TypedTransaction::Eip1559(eip1559_tx) = typed_transaction else {
            unreachable!("anchor tx is eip1559 by protocol spec")
        };

        // Use the deterministic signer (K = 1 or K = 2) to sign the transaction.
        match sign_hash_deterministic(TAIKO_GOLDEN_TOUCH_PRIVATE_KEY, eip1559_tx.signature_hash()) {
            Ok(signature) => Ok(TxEnvelope::Eip1559(eip1559_tx.into_signed(signature))),
            Err(e) => {
                let msg = format!("failed to sign anchor tx with deterministic signer: {:?}", e);
                Err(TransportError::from(TransportErrorKind::custom_str(&msg)))
            }
        }
    }

    /// Decodes the provided anchor transaction data
    pub fn decode_anchor_tx_data(data: &[u8]) -> SolResult<TaikoAnchorTxInput> {
        let tx_data = anchorV3Call::abi_decode(data)?;
        Ok(TaikoAnchorTxInput::from(tx_data))
    }

    /// Decodes the anchor transaction from the provided L2 block.
    ///
    /// Returns an error if:
    /// - the block is empty.
    /// - the first transaction in the block is not from the golden touch account.
    /// - the transaction data cannot be decoded properly.
    pub fn decode_anchor_tx_from_full_block(
        block: &Block,
    ) -> Result<TaikoAnchorTxInput, TaikoAnchorError> {
        let Some(first) = block.transactions.as_transactions().and_then(|txs| txs.first()) else {
            return Err(TaikoAnchorError::AnchorTransactionNotFound);
        };

        if first.inner.signer() != TAIKO_GOLDEN_TOUCH_ACCOUNT {
            return Err(TaikoAnchorError::InvalidSigner {
                got: first.inner.signer(),
                expected: TAIKO_GOLDEN_TOUCH_ACCOUNT,
            });
        }

        Ok(Self::decode_anchor_tx_data(first.input())?)
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    interface ITaikoAnchor {
        struct BaseFeeConfig {
            uint8 adjustmentQuotient;
            uint8 sharingPctg;
            uint32 gasIssuancePerSecond;
            uint64 minGasExcess;
            uint32 maxGasIssuancePerBlock;
        }

        /// @notice Anchors the latest L1 block details to L2 for cross-layer
        /// message verification.
        /// @dev The gas limit for this transaction must be set to 1,000,000 gas.
        /// @dev This function can be called freely as the golden touch private key is publicly known,
        /// but the Taiko node guarantees the first transaction of each block is always this anchor
        /// transaction, and any subsequent calls will revert with L2_PUBLIC_INPUT_HASH_MISMATCH.
        /// @param _anchorBlockId The `anchorBlockId` value in this block's metadata.
        /// @param _anchorStateRoot The state root for the L1 block with id equals `_anchorBlockId`.
        /// @param _parentGasUsed The gas used in the parent block.
        /// @param _baseFeeConfig The base fee configuration.
        /// @param _signalSlots The signal slots to mark as received.
        function anchorV3(
            uint64 _anchorBlockId,
            bytes32 _anchorStateRoot,
            uint32 _parentGasUsed,
            BaseFeeConfig calldata _baseFeeConfig,
            bytes32[] calldata _signalSlots
        )
            external;

        /// @notice Calculates the base fee and gas excess using EIP-1559 configuration for the given
        /// parameters.
        /// @param _parentGasUsed Gas used in the parent block.
        /// @param _baseFeeConfig Configuration parameters for base fee calculation.
        /// @return basefee_ The calculated EIP-1559 base fee per gas.
        /// @return newGasTarget_ The new gas target value.
        /// @return newGasExcess_ The new gas excess value.
        function getBasefeeV2(
            uint32 _parentGasUsed,
            uint64 _blockTimestamp,
            BaseFeeConfig calldata _baseFeeConfig
        )
            public
            view
            returns (uint256 basefee_, uint64 newGasTarget_, uint64 newGasExcess_);
    }
}

/// Data structure returned by `getBasefeeV2`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseFeeResult {
    /// The calculated EIP-1559 base fee per gas.
    pub base_fee: U256,
    /// The new gas target value.
    pub new_gas_target: u64,
    /// The new gas excess value.
    pub new_gas_excess: u64,
}

// Same type, different namespace
impl From<super::inbox::ITaikoInbox::BaseFeeConfig> for BaseFeeConfig {
    fn from(config: super::inbox::ITaikoInbox::BaseFeeConfig) -> Self {
        Self {
            adjustmentQuotient: config.adjustmentQuotient,
            sharingPctg: config.sharingPctg,
            gasIssuancePerSecond: config.gasIssuancePerSecond,
            minGasExcess: config.minGasExcess,
            maxGasIssuancePerBlock: config.maxGasIssuancePerBlock,
        }
    }
}

/// Helper struct that matches a [`anchorV3Call`] input.
#[derive(Debug, Clone)]
pub struct TaikoAnchorTxInput {
    /// Anchor block number
    pub anchor_block_id: u64,
    /// Anchor block state root
    pub anchor_state_root: B256,
    /// Parent L2 block gas used
    pub parent_gas_used: u32,
    /// L2 Base fee configuration
    pub base_fee_config: BaseFeeConfig,
    /// Signal slots
    pub signal_slots: Vec<B256>,
}

impl From<anchorV3Call> for TaikoAnchorTxInput {
    fn from(anchor: anchorV3Call) -> Self {
        Self {
            anchor_block_id: anchor._anchorBlockId,
            anchor_state_root: anchor._anchorStateRoot,
            parent_gas_used: anchor._parentGasUsed,
            base_fee_config: anchor._baseFeeConfig,
            signal_slots: anchor._signalSlots,
        }
    }
}

impl From<TaikoAnchorTxInput> for anchorV3Call {
    fn from(value: TaikoAnchorTxInput) -> Self {
        Self::new((
            value.anchor_block_id,
            value.anchor_state_root,
            value.parent_gas_used,
            value.base_fee_config,
            value.signal_slots,
        ))
    }
}
