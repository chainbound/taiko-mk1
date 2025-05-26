use std::fmt::Debug;

use alloy::{
    consensus::BlobTransactionSidecar,
    rpc::{client::ClientBuilder, types::TransactionRequest},
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use alloy_primitives::{Address, Bytes, aliases::U96};
use derive_more::derive::Deref;
use mk1_primitives::retries::DEFAULT_RETRY_LAYER;
use url::Url;

use super::inbox::ITaikoInbox::{BatchParams, ITaikoInboxInstance};

use crate::{
    WalletProviderWithSimpleNonceManager, new_wallet_provider_with_simple_nonce_management,
};

/// A wrapper over a `IPreconfRouter` contract that exposes various utility methods.
///
/// `PreconfRouter` shares the same batch posting interface of `TaikoInbox`.
#[derive(Debug, Clone, Deref)]
pub struct PreconfRouter(ITaikoInboxInstance<WalletProviderWithSimpleNonceManager>);

impl PreconfRouter {
    /// Create a new `PreconfRouter` instance at the given contract address.
    pub fn new<U: Into<Url>>(el_client_url: U, address: Address, wallet: PrivateKeySigner) -> Self {
        // Instantiate the RetryBackoffLayer with the configuration
        let rpc_client =
            ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(el_client_url.into());

        let provider = new_wallet_provider_with_simple_nonce_management(rpc_client, wallet);

        Self(ITaikoInboxInstance::new(address, provider))
    }

    /// Returns a [`TransactionRequest`] for the `proposeBatch` function.
    pub fn propose_batch_tx_request(
        &self,
        params: BatchParams,
        tx_list: Bytes,
        sidecar: BlobTransactionSidecar,
    ) -> TransactionRequest {
        // TaikoWrapper: X = forced inclusion batch, Y = sequencer proposed batch
        //
        // Reference: <https://github.com/taikoxyz/taiko-mono/blob/adaf808a4baebf231953893a54e1e51c91bb88b7/packages/protocol/contracts/layer1/forced-inclusion/TaikoWrapper.sol#L84-L94>
        let encoded_params = Bytes::from((Bytes::new(), params.abi_encode()).abi_encode_params());

        self.proposeBatch(encoded_params, tx_list).sidecar(sidecar).into_transaction_request()
    }

    /// Returns a [`TransactionRequest`] for the `proposeBatchWithExpectedLastBlockId` function.
    pub fn propose_batch_with_expected_last_block_id_tx_request(
        &self,
        params: BatchParams,
        tx_list: Bytes,
        expected_last_block_id: u64,
        sidecar: BlobTransactionSidecar,
    ) -> TransactionRequest {
        let encoded_params = Bytes::from((Bytes::new(), params.abi_encode()).abi_encode_params());

        self.proposeBatchWithExpectedLastBlockId(
            encoded_params,
            tx_list,
            U96::from(expected_last_block_id),
        )
        .sidecar(sidecar)
        .into_transaction_request()
    }
}
