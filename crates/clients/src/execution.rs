use std::{any::type_name, marker::PhantomData, time::Duration};

use alloy::{
    network::Ethereum,
    providers::{
        Provider, ProviderBuilder, RootProvider, fillers::FillProvider,
        utils::JoinedRecommendedFillers,
    },
    rpc::{
        client::{ClientBuilder, RpcClient},
        types::{Block, BlockNumberOrTag, BlockTransactionsKind, Filter, Header, Log, SyncStatus},
    },
    transports::{TransportErrorKind, TransportResult},
};
use alloy_primitives::{B256, Bytes, U64};
use alloy_rpc_types_trace::geth::{
    CallConfig, CallFrame, GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingOptions,
};
use alloy_sol_types::{SolEvent, SolInterface};
use derive_more::derive::{Deref, DerefMut};
use mk1_chainio::DefaultProvider;
use mk1_primitives::{
    retries::{DEFAULT_RETRY_LAYER, RetryWsConnect},
    taiko::L1Origin,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::sleep,
};
use tokio_stream::{
    StreamExt,
    adapters::Skip,
    wrappers::{ReceiverStream, WatchStream},
};
use tracing::{debug, error, warn};
use url::Url;

/// An HTTP-based JSON-RPC execution client provider that supports batching.
///
/// This struct is a wrapper over an inner [`RootProvider`] and extends it with
/// methods that are relevant to the Bolt state.
#[derive(Clone, Debug, Deref, DerefMut)]
pub struct ExecutionClient<CHAIN = Ethereum> {
    /// The custom RPC client that allows us to add custom batching and extend the provider.
    rpc: RpcClient,
    /// The inner provider that implements all the JSON-RPC methods, that can be
    /// easily used via dereferencing this struct.
    #[deref]
    #[deref_mut]
    inner: FillProvider<JoinedRecommendedFillers, RootProvider, Ethereum>,
    /// The URL of the WebSocket endpoint for the execution client.
    ws_provider: DefaultProvider,
    /// The chain type.
    _chain: PhantomData<CHAIN>,
}

/// The chain type for Taiko nodes.
///
/// This is useful to define methods that are available only on taiko-geth clients.
#[derive(Clone, Copy, Debug, Default)]
pub struct Taiko;

/// The type alias for a Taiko execution client.
pub type TaikoExecutionClient = ExecutionClient<Taiko>;

impl TaikoExecutionClient {
    /// TAIKO-SPECIFIC: returns the L1 origin of the block with the given ID.
    ///
    /// Ref: <https://github.com/taikoxyz/taiko-geth/blob/v1.8.0/eth/taiko_api_backend.go#L50>
    pub async fn l1_origin_by_id(&self, block_id: u64) -> TransportResult<L1Origin> {
        self.rpc.request("taiko_l1OriginByID", block_id).await
    }

    /// TAIKO-SPECIFIC: returns the L1 origin of the last L2 block that was synced
    /// by the node. this doesn't include preconfirmed blocks, so any block past this point is
    /// to be considered unsafe and should be kept in memory in case we need to propose it.
    ///
    /// NOTE: this is not safe to use at all times, especially after a beacon sync has finished.
    /// Prefer the option of fetching from the protocol via `TaikoInbox` bindings when possible.
    ///
    /// Ref: <https://github.com/taikoxyz/taiko-geth/blob/v1.8.0/eth/taiko_api_backend.go#L27>
    pub async fn head_l1_origin_unsafe(&self) -> TransportResult<Option<L1Origin>> {
        let res: TransportResult<L1Origin> = self.rpc.request("taiko_headL1Origin", ()).await;
        match res {
            Ok(origin) => Ok(Some(origin)),
            Err(e) => {
                if e.to_string().contains("not found") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl<CHAIN> ExecutionClient<CHAIN> {
    /// Create a new [`ExecutionClient`] with the given HTTP and WS URLs.
    pub async fn new<U: Into<Url>>(http_url: U, ws_url: U) -> TransportResult<Self> {
        let http_url = http_url.into();
        let rpc = ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).http(http_url.clone());

        let inner = ProviderBuilder::new().connect_client(rpc.clone());

        let ws_connection = RetryWsConnect::from_url(ws_url);
        let ws_client =
            ClientBuilder::default().layer(DEFAULT_RETRY_LAYER).pubsub(ws_connection).await?;
        let ws_provider = ProviderBuilder::new().connect_client(ws_client);

        Ok(Self { rpc, inner, ws_provider, _chain: PhantomData })
    }

    /// Get the latest block number
    pub async fn get_head(&self) -> TransportResult<u64> {
        let result: U64 = self.rpc.request("eth_blockNumber", ()).await?;

        Ok(result.to())
    }

    /// Get the block with the given number. If `None`, the latest block is returned.
    pub async fn get_block(&self, block_number: Option<u64>, full: bool) -> TransportResult<Block> {
        let tag = block_number.map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number);

        self.rpc.request("eth_getBlockByNumber", (tag, full)).await
    }

    /// Get the header of the block with the given number. If `None`, the latest block is returned.
    pub async fn get_header(&self, block_number: Option<u64>) -> TransportResult<Header> {
        let tag = block_number.map_or(BlockNumberOrTag::Latest, BlockNumberOrTag::Number);

        let block: Option<Header> = self.rpc.request("eth_getHeaderByNumber", vec![tag]).await?;
        block.ok_or_else(|| TransportErrorKind::custom_str(&format!("Header not found: {}", tag)))
    }

    /// Send a raw transaction to the network.
    pub async fn send_raw_transaction(&self, raw: Bytes) -> TransportResult<B256> {
        self.rpc.request("eth_sendRawTransaction", [raw]).await
    }

    /// Check if the client is synced. Returns `true` if the client is synced.
    pub async fn is_synced(&self) -> TransportResult<bool> {
        let status = self.syncing().await?;
        Ok(matches!(status, SyncStatus::None))
    }

    /// Spawn a background task that will handle the subscription, and return a watch stream
    /// yielding the hydrated blocks, alongside the handle of the task.
    pub fn subscribe_blocks(
        &self,
        kind: BlockTransactionsKind,
    ) -> (Skip<WatchStream<Block>>, JoinHandle<()>) {
        let ws = self.ws_provider.clone();
        let (full_block_tx, full_block_rx) = watch::channel(Block::default());
        // NOTE: skip the default block
        let stream = WatchStream::new(full_block_rx).skip(1);

        let handle = tokio::spawn(async move {
            loop {
                let Ok(mut sub) = ws.subscribe_blocks().await else {
                    error!("Failed to subscribe to new blocks");
                    sleep(Duration::from_secs(2)).await;
                    continue;
                };

                while let Ok(header) = sub.recv().await {
                    match ws.get_block_by_hash(header.hash).kind(kind).await {
                        Ok(Some(full_block)) => {
                            full_block_tx.send(full_block).expect("Failed to send full block");
                        }
                        Ok(None) => {
                            error!(block_number = %header.number, block_hash = %header.hash, "EL client block not found");
                        }
                        Err(err) => {
                            error!(block_number = %header.number, ?err,  "Failed to fetch full block for hash from EL client");
                        }
                    }
                }

                warn!("Subscription to new blocks closed, retrying...");
            }
        });

        (stream, handle)
    }

    /// Subscribe to new events based on a filter.
    ///
    /// This function will spawn a background task that will handle the subscription and
    /// dispatch events to the returned receiver stream.
    pub fn subscribe_events(&self, filter: Filter) -> ReceiverStream<Log> {
        let ws = self.ws_provider.clone();
        let (event_tx, event_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let Ok(mut sub) = ws.subscribe_logs(&filter).await else {
                    error!("Failed to subscribe to new events");
                    sleep(Duration::from_secs(2)).await;
                    continue;
                };

                debug!(address = ?filter.address, topic = ?filter.topics.first(), "Subscribed to new events");

                while let Ok(log) = sub.recv().await {
                    if let Err(err) = event_tx.send(log).await {
                        error!(?err, "Failed to send event");
                    }
                }

                warn!("Subscription to new events closed, retrying...");
            }
        });

        ReceiverStream::new(event_rx)
    }

    /// Subscribe to log events of the given type.
    ///
    /// Returns a tuple containing the raw log and the decoded event data.
    pub fn subscribe_log_event<T: SolEvent + Send + 'static>(
        &self,
        filter: Filter,
    ) -> ReceiverStream<(Log, T)> {
        let (tx, rx) = mpsc::channel(64);

        let mut stream = self.subscribe_events(filter);

        tokio::spawn(async move {
            let event_name = type_name::<T>();
            while let Some(event) = stream.next().await {
                match event.log_decode::<T>() {
                    Ok(decoded_log) => {
                        let data = decoded_log.into_inner().data;
                        if tx.send((event, data)).await.is_err() {
                            error!("Failed to send {event_name} event");
                        }
                    }
                    Err(e) => {
                        let topic0 = event.topic0();
                        error!(?topic0, ?e, "Error while decoding {event_name} event");
                    }
                }
            }
        });

        ReceiverStream::new(rx)
    }

    /// Runs a simple `debug_traceTransaction` RPC call to establish the revert reason of a
    /// transaction, if any.
    ///
    /// NOTE: assumes a client compatible with the `debug_traceTransaction` RPC call
    pub async fn debug_revert_reason(&self, tx_hash: B256) -> TransportResult<RevertReasonTrace> {
        let opts = GethDebugTracingOptions::default()
            .with_tracer(GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::CallTracer))
            .with_config(CallConfig::default().only_top_call());

        let trace: CallFrame = self.rpc.request("debug_traceTransaction", (tx_hash, opts)).await?;

        // Return the revert reason if it exists, otherwise return the output bytes
        // that can be decoded into a contract error. As a last resort, return `Unknown`.
        match trace.revert_reason {
            Some(reason) => Ok(RevertReasonTrace::String(reason)),
            None => match trace.output {
                Some(output) => Ok(RevertReasonTrace::ContractBytes(output)),
                None => Ok(RevertReasonTrace::Unknown),
            },
        }
    }
}

/// The result of a `debug_traceTransaction` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertReasonTrace {
    /// The revert reason is a string.
    String(String),
    /// The output is a bytes object.
    ContractBytes(Bytes),
    /// The reason is unknown
    Unknown,
}

impl RevertReasonTrace {
    /// Try to decode the revert reason into a specific interface, if possible.
    pub fn try_decode<I: SolInterface + std::fmt::Debug>(self) -> String {
        match self {
            Self::String(reason) => reason,
            Self::ContractBytes(bytes) => format!("{:?}", I::abi_decode(&bytes)),
            Self::Unknown => "unknown".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use mk1_chainio::taiko::inbox::ITaikoInbox::ITaikoInboxErrors;

    use super::*;

    /// This test is ignored because it requires a local devnet running.
    /// It can still be useful for manually debugging reverted transactions.
    #[ignore]
    #[tokio::test]
    async fn test_debug_revert_reason() {
        let url = Url::parse("http://remotesmol:46553").unwrap();
        let ws = Url::parse("ws://remotesmol:46554").unwrap();
        let client = ExecutionClient::<Ethereum>::new(url, ws).await.unwrap();

        let tx_hash = "0xafe2f60a5cfa610162e3246adca56b6d679d3d68739675831a22967c8e4244ef";

        let reason = client.debug_revert_reason(tx_hash.parse().unwrap()).await.unwrap();

        let reason = reason.try_decode::<ITaikoInboxErrors>();

        println!("reason: {}", reason);
    }
}
