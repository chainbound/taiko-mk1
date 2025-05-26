use std::{borrow::Cow, time::Duration};

use alloy::{
    providers::{Provider, RootProvider},
    rpc::client::RpcClient,
    transports::{
        RpcError, TransportErrorKind, TransportResult,
        http::{Http, reqwest::Url},
    },
};
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::JwtSecret;
use alloy_transport_http::{
    AuthLayer, HyperClient,
    hyper_util::{client::legacy::Client, rt::TokioExecutor},
};
use derive_more::derive::Deref;
use http_body_util::Full;
use mk1_primitives::{
    retries::is_connection_refused,
    taiko::builder::{PreBuiltTxList, TxPoolContentParams},
};
use serde_json::Value;
use tokio_retry::{RetryIf, strategy::ExponentialBackoff};
use tower::ServiceBuilder;

const TAIKO_AUTH_NAMESPACE: &str = "taikoAuth_";
const TX_POOL_CONTENT_WITH_MIN_TIP: &str = "txPoolContentWithMinTip";

/// The [`EngineClient`] is responsible for interacting with the engine API via HTTP.
/// The inner transport uses a JWT [`AuthLayer`] to authenticate requests.
#[derive(Debug, Clone, Deref)]
pub struct EngineClient {
    inner: RootProvider,
}

impl EngineClient {
    /// Creates a new [`EngineClient`] from the provided [Url] and [`JwtSecret`].
    pub fn new(url: Url, jwt: JwtSecret) -> Self {
        let hyper_client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();

        let auth_layer = AuthLayer::new(jwt);
        let service = ServiceBuilder::new().layer(auth_layer).service(hyper_client);

        let layer_transport = HyperClient::<Full<Bytes>, _>::with_service(service);
        let http_hyper = Http::with_client(layer_transport, url);
        let rpc_client = RpcClient::new(http_hyper, true);
        let inner = RootProvider::new(rpc_client);

        Self { inner }
    }

    /// Call the `taikoAuth_txPoolContentWithMinTip` method on the target.
    /// * Returns an array of transactions lists corresponding to L2 blocks.
    /// * Returns None if the mempool is empty.
    ///
    /// Reference: <https://github.com/taikoxyz/taiko-geth/blob/2448fb97a8b873c7bd7c0051cd83aaea339050e0/eth/taiko_api_backend.go#L118-L147>
    pub async fn tx_pool_content_with_min_tip(
        &self,
        params: TxPoolContentParams,
    ) -> TransportResult<Option<Vec<PreBuiltTxList>>> {
        let retry_strategy =
            ExponentialBackoff::from_millis(10).max_delay(Duration::from_millis(1_000));

        let client = self.client();
        let method = Cow::from(format!("{}{}", TAIKO_AUTH_NAMESPACE, TX_POOL_CONTENT_WITH_MIN_TIP));

        // NOTE(thedevbirb): couldn't make retry layer work with auth layer, so retries are handled
        // manually here.
        RetryIf::spawn(
            retry_strategy,
            || async {
                client
                    .request::<Vec<Value>, Option<Vec<PreBuiltTxList>>>(
                        method.clone(),
                        params.clone().into_rpc_params(),
                    )
                    .await
            },
            |res: &RpcError<TransportErrorKind>| {
                if let RpcError::Transport(e) = res {
                    e.is_retry_err() || is_connection_refused(e)
                } else {
                    false
                }
            },
        )
        .await
    }
}
