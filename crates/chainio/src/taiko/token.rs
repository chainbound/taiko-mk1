use alloy::{
    contract::{Error as ContractError, Result as ContractResult},
    rpc::client::ClientBuilder,
    signers::local::PrivateKeySigner,
    sol,
    transports::TransportErrorKind,
};
use alloy_primitives::{Address, U256};
use derive_more::derive::Deref;
use mk1_primitives::retries::DEFAULT_RETRY_LAYER;
use tracing::{debug, error};
use url::Url;

use crate::{
    WalletProviderWithSimpleNonceManager, new_wallet_provider_with_simple_nonce_management,
};

use ITaikoToken::ITaikoTokenInstance;

/// A wrapper over the `TaikoToken` contract.
#[derive(Debug, Clone, Deref)]
pub struct TaikoToken {
    /// The `TaikoToken` contract instance
    token: ITaikoTokenInstance<WalletProviderWithSimpleNonceManager>,
}

impl TaikoToken {
    /// Create a new `TaikoToken` instance at the given contract address.
    pub fn new<U: Into<Url>>(
        el_client_url: U,
        token_address: Address,
        wallet: PrivateKeySigner,
    ) -> Self {
        let client = ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(el_client_url.into());

        let provider = new_wallet_provider_with_simple_nonce_management(client, wallet);

        Self { token: ITaikoTokenInstance::new(token_address, provider) }
    }

    /// Check if the spender has sufficient allowance for TAIKO tokens and approve if needed.
    /// Uses a large approval amount to avoid frequent re-approvals.
    pub async fn check_and_approve(
        &self,
        spender: Address,
        min_allowance: U256,
        operator: Address,
    ) -> ContractResult<()> {
        // Check current allowance
        let allowance = self.token.allowance(operator, spender).call().await?;

        if allowance < min_allowance {
            debug!(%operator, %spender, "Approving spender to spend $TAIKO");

            // Approve spender to spend infinite $TAIKO to avoid frequent re-approvals
            let pending = self.token.approve(spender, U256::MAX).send().await?;
            let receipt = pending.get_receipt().await?;

            if receipt.status() {
                debug!(%operator, %spender, "Approved spender to spend infinite $TAIKO");
            } else {
                error!(%operator, %spender, "Failed to approve spender to spend $TAIKO");
                return Err(ContractError::TransportError(TransportErrorKind::custom_str(
                    "Failed to approve spender to spend $TAIKO",
                )))
            }
        }

        Ok(())
    }

    /// Returns the amount of tokens owned by an account.
    pub async fn get_balance_of(&self, account: Address) -> ContractResult<U256> {
        let balance_result = self.token.balanceOf(account).call().await?;
        Ok(balance_result)
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    interface ITaikoToken {
        error TT_INVALID_PARAM();

        /// @notice Returns the amount of tokens approved by the owner for the spender.
        /// @param owner The address of the token owner.
        /// @param spender The address of the token spender.
        /// @return The amount of tokens approved.
        function allowance(address owner, address spender) external view returns (uint256);

        /// @notice Approves the spender to spend tokens on behalf of the owner.
        /// @param spender The address of the token spender.
        /// @param amount The amount of tokens to approve.
        /// @return True if the approval was successful.
        function approve(address spender, uint256 amount) external returns (bool);

        /// @notice Returns the amount of tokens owned by an account.
        /// @param account The address of the account.
        /// @return The amount of tokens owned.
        function balanceOf(address account) external view returns (uint256);
    }
}
