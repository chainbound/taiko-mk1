use ITaikoWrapper::ITaikoWrapperInstance;
use alloy::{
    contract::Result as ContractResult, primitives::Address, providers::ProviderBuilder,
    rpc::client::ClientBuilder, sol,
};
use derive_more::derive::Deref;
use mk1_primitives::retries::DEFAULT_RETRY_LAYER;
use url::Url;

use crate::DefaultProvider;

/// A wrapper over a `TaikoWrapper.sol` contract.
#[derive(Debug, Clone, Deref)]
pub struct TaikoWrapper(ITaikoWrapperInstance<DefaultProvider>);

impl TaikoWrapper {
    /// Create a new `TaikoWrapper` instance at the given contract address.
    pub fn new<U: Into<Url>>(el_client_url: U, address: Address) -> Self {
        let client = ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(el_client_url.into());

        let provider = ProviderBuilder::new().connect_client(client);

        Self(ITaikoWrapperInstance::new(address, provider))
    }

    /// Returns the address of the preconf router contract.
    pub async fn get_preconf_router(&self) -> ContractResult<Address> {
        self.0.preconfRouter().call().await
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    interface ITaikoWrapper {
        address public immutable preconfRouter;
    }
}
