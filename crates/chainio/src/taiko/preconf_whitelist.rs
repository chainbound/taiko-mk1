use IPreconfWhitelist::{
    IPreconfWhitelistErrors, IPreconfWhitelistInstance, OperatorAdded_0, OperatorAdded_1,
    OperatorRemoved_0, OperatorRemoved_1,
};
use alloy::{
    contract::Result as ContractResult,
    providers::ProviderBuilder,
    rpc::{client::ClientBuilder, types::Filter},
};
use alloy_primitives::Address;
use alloy_sol_macro::sol;
use alloy_sol_types::{Error as SolError, SolEvent as _};
use mk1_primitives::{retries::DEFAULT_RETRY_LAYER, time::Timestamp};
use url::Url;

use crate::{DefaultProvider, try_parse_contract_error};

/// A wrapper over a `IPreconfWhitelist` contract that exposes various utility methods.
#[derive(Debug, Clone)]
pub struct TaikoPreconfWhitelist(IPreconfWhitelistInstance<DefaultProvider>);

impl TaikoPreconfWhitelist {
    /// Create a new `TaikoPreconfWhitelist` instance at the given contract address.
    pub fn from_address<U: Into<Url>>(el_client_url: U, address: Address) -> Self {
        let client = ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(el_client_url.into());
        let provider = ProviderBuilder::new().connect_client(client);
        Self(IPreconfWhitelistInstance::new(address, provider))
    }

    /// Get the operator for the current epoch.
    pub async fn get_operator_for_current_epoch(&self) -> ContractResult<Address> {
        match self.0.getOperatorForCurrentEpoch().call().await {
            Ok(result) => Ok(result),
            Err(err) => {
                let decoded_error = try_parse_contract_error::<IPreconfWhitelistErrors>(err)?;
                Err(SolError::custom(format!("{:?}", decoded_error)).into())
            }
        }
    }

    /// Get the operator for the next epoch.
    pub async fn get_operator_for_next_epoch(&self) -> ContractResult<Address> {
        match self.0.getOperatorForNextEpoch().call().await {
            Ok(result) => Ok(result),
            Err(err) => {
                let decoded_error = try_parse_contract_error::<IPreconfWhitelistErrors>(err)?;
                Err(SolError::custom(format!("{:?}", decoded_error)).into())
            }
        }
    }

    /// Check if an address is active in the whitelist for a given epoch.
    ///
    /// Note: "active" in the contract just means that the operator is able to be selected as the
    /// sequencer, not that it is currently the sequencer.
    pub async fn is_whitelisted(
        &self,
        address: Address,
        epoch_timestamp: Timestamp,
    ) -> ContractResult<bool> {
        match self.0.isOperatorActive(address, epoch_timestamp).call().await {
            Ok(result) => Ok(result),
            Err(err) => {
                // Patch: handle the old whitelist contract implementation
                if err.to_string().contains("execution reverted") {
                    self.0.isOperator(address).call().await
                } else {
                    let decoded_error = try_parse_contract_error::<IPreconfWhitelistErrors>(err)?;
                    Err(SolError::custom(format!("{:?}", decoded_error)).into())
                }
            }
        }
    }

    /// Return a filter for both the [`IPreconfWhitelist::OperatorAdded`] and
    /// [`IPreconfWhitelist::OperatorRemoved`] events.
    ///
    /// NOTE: this will match for both version of the events. The older version just had the
    /// `operator` topic, while the new one has also a non-indexed timestamp argument.
    pub fn operator_changed_filter(&self) -> Filter {
        Filter::new().address(*self.0.address()).events(vec![
            OperatorAdded_0::SIGNATURE,
            OperatorRemoved_0::SIGNATURE,
            OperatorAdded_1::SIGNATURE,
            OperatorRemoved_1::SIGNATURE,
        ])
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    interface IPreconfWhitelist {
        error InvalidOperatorIndex();
        error InvalidOperatorCount();
        error InvalidOperatorAddress();
        error OperatorAlreadyExists();
        error OperatorNotAvailableYet();

        /// @notice Emitted when a new operator is added to the whitelist.
        /// @param operator The address of the operator that was added.
        ///
        /// NOTE: this is an older version of the event, kept for backwards compatibility.
        event OperatorAdded(address indexed operator);

        /// @notice Emitted when a new operator is added to the whitelist.
        /// @param operator The address of the operator that was added.
        /// @param activeSince The timestamp when the operator became active.
        event OperatorAdded(address indexed operator, uint256 activeSince);

        /// @notice Emitted when an operator is removed from the whitelist.
        /// @param operator The address of the operator that was removed.
        ///
        /// NOTE: this is an older version of the event, kept for backwards compatibility.
        event OperatorRemoved(address indexed operator);

        /// @notice Emitted when an operator is removed from the whitelist.
        /// @param operator The address of the operator that was removed.
        /// @param inactiveSince The timestamp when the operator became inactive.
        event OperatorRemoved(address indexed operator, uint256 inactiveSince);

        /// @notice Adds a new operator to the whitelist.
        /// @param _operatorAddress The address of the operator to be added.
        /// @dev Only callable by the owner or an authorized address.
        function addOperator(address _operatorAddress) external;

        /// @notice Removes an operator from the whitelist.
        /// @param _operatorId The ID of the operator to be removed.
        /// @dev Only callable by the owner or an authorized address.
        /// @dev Reverts if the operator ID does not exist.
        function removeOperator(uint256 _operatorId) external;

        /// @notice Retrieves the address of the operator for the current epoch.
        /// @dev Uses the beacon block root of the first block in the last epoch as the source
        ///      of randomness.
        /// @return The address of the operator.
        function getOperatorForCurrentEpoch() external view returns (address operator);

        /// @notice Retrieves the address of the operator for the next epoch.
        /// @dev Uses the beacon block root of the first block in the current epoch as the source
        ///      of randomness.
        /// @return The address of the operator.
        function getOperatorForNextEpoch() external view returns (address operator);

        /// @notice Check if an address is an active operator in the whitelist.
        /// @param operator The address to check.
        /// @param epochTimestamp The timestamp of the epoch to check.
        /// @return True if the address is an active operator in the given epoch.
        function isOperatorActive(address operator, uint64 epochTimestamp) external view returns (bool);

        /// @notice The OLD whitelist method for checking if an address is whitelisted.
        /// @dev This is kept for backwards compatibility.
        function isOperator(address operator) external view returns (bool);
    }
}
