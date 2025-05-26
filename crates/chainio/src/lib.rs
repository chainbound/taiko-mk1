#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Chain I/O module to interact with smart contracts on EVM chains.

use alloy::{
    contract::Error as ContractError,
    network::EthereumWallet,
    providers::{
        RootProvider,
        fillers::{
            BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
            SimpleNonceManager, WalletFiller,
        },
        utils::JoinedRecommendedFillers,
    },
    rpc::client::RpcClient,
    signers::local::PrivateKeySigner,
    transports::TransportError,
};
use alloy_primitives::Bytes;
use alloy_sol_types::SolInterface;

/// Taiko contract bindings
pub mod taiko;

/// Alias to the joined recommended fillers + wallet filler for Ethereum wallets.
pub type JoinedWalletFillers = JoinFill<JoinedRecommendedFillers, WalletFiller<EthereumWallet>>;

/// Alias to the default wallet provider with all recommended fillers (read + write).
pub type DefaultWalletProvider = FillProvider<JoinedWalletFillers, RootProvider>;

/// Alias to the default provider with all recommended fillers (read-only).
pub type DefaultProvider = FillProvider<JoinedRecommendedFillers, RootProvider>;

/// Alias to the default fillers with a simple nonce manager instead of the default cached one.
pub type DefaultFillersWithSimpleNonceManager = JoinFill<
    GasFiller,
    JoinFill<BlobGasFiller, JoinFill<NonceFiller<SimpleNonceManager>, ChainIdFiller>>,
>;

/// Alias to the wallet provider with recommended fillers (read + write) and a simple nonce manager.
pub type WalletProviderWithSimpleNonceManager = FillProvider<
    JoinFill<DefaultFillersWithSimpleNonceManager, WalletFiller<EthereumWallet>>,
    RootProvider,
>;

/// Create a new wallet provider with a simple nonce manager instead of the default cached one.
/// We have to build the entire provider fill stack manually :)
///
/// This is necessary after alloy 0.14.0 because of: <https://github.com/alloy-rs/alloy/pull/2289>
///
/// (NICO): Praying to the Alloy gods to fix this 🙏
pub fn new_wallet_provider_with_simple_nonce_management(
    rpc_client: RpcClient,
    wallet: PrivateKeySigner,
) -> WalletProviderWithSimpleNonceManager {
    FillProvider::new(
        RootProvider::new(rpc_client),
        JoinFill::new(
            JoinFill::new(
                GasFiller,
                JoinFill::new(
                    BlobGasFiller,
                    JoinFill::new(
                        NonceFiller::new(SimpleNonceManager::default()),
                        ChainIdFiller::default(),
                    ),
                ),
            ),
            WalletFiller::new(wallet.into()),
        ),
    )
}

/// Try to decode a contract error into a specific Solidity error interface.
/// If the error cannot be decoded or it is not a contract error, return the original error.
///
/// Example usage:
///
/// ```ignore
/// sol! {
///    library ErrorLib {
///       error SomeError(uint256 code);
///    }
/// }
///
/// // call a contract that may return an error with the SomeError interface
/// let returndata = match myContract.call().await {
///    Ok(returndata) => returndata,
///    Err(err) => {
///         let decoded_error = try_decode_contract_error::<ErrorLib::ErrorLibError>(err)?;
///        // handle the decoded error however you want; for example, return it
///         return Err(decoded_error);
///    },
/// }
/// ```
///
/// See also [`ContractError::as_decoded_interface_error`] for more details.
pub fn try_parse_contract_error<I: SolInterface>(error: ContractError) -> Result<I, ContractError> {
    error.as_decoded_interface_error::<I>().ok_or(error)
}

/// The result of trying to parse a transport error into a specific interface.
#[derive(Debug)]
pub enum TryParseTransportErrorResult<I: SolInterface> {
    /// The error was successfully decoded into the specified interface.
    Decoded(I),
    /// The error was not decoded but the revert data was extracted.
    UnknownSelector(Bytes),
    /// The error was not decoded and the revert data was not extracted.
    Original(TransportError),
}

/// The same as [`try_parse_contract_error`] but if you already know it is [`TransportError`].
pub fn try_parse_transport_error<I: SolInterface>(
    error: TransportError,
) -> TryParseTransportErrorResult<I> {
    // Performs the same operation as [`ContractError::as_decoded_interface_error`].
    // This unwrapping is needed because otherwise we can't clone the original error.
    let revert_data = error.as_error_resp().and_then(|e| e.as_revert_data());
    let decoded = revert_data.as_ref().and_then(|data| I::abi_decode(data).ok());

    if let Some(decoded) = decoded {
        TryParseTransportErrorResult::Decoded(decoded)
    } else if let Some(revert_data) = revert_data {
        TryParseTransportErrorResult::UnknownSelector(revert_data)
    } else {
        TryParseTransportErrorResult::Original(error)
    }
}
