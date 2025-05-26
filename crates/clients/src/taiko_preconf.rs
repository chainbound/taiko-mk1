use std::time::Duration;

use alloy::rpc::types::Header;
use alloy_rpc_types_engine::{Claims, JwtSecret};
use mk1_primitives::{
    retries::is_connection_refused,
    taiko::builder::{
        BuildPreconfBlockRequestBody, BuildPreconfBlockResponseBody, TaikoPreconfStatus,
        TaikoPreconfWsEvent,
    },
    time::current_timestamp_seconds,
};
use reqwest::{
    Client,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::Value;
use tokio::{sync::mpsc, time::sleep};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, warn};
use url::Url;

/// Errors that can occur when interacting with the Taiko preconf API.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum TaikoPreconfClientError {
    #[error("URL parse error: {0}")]
    ParseUrl(#[from] url::ParseError),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("WebSocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("Jwt error: {0}")]
    Jwt(String),
    #[error("Serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Build preconf block error: {0}")]
    Build(String),
    #[error("Operation failed after retries. Last error: {last_error}")]
    RetriesExhausted { last_error: Box<TaikoPreconfClientError> },
}

impl TaikoPreconfClientError {
    /// Returns whether this error should trigger a retry.
    fn is_retryable(&self) -> bool {
        match self {
            Self::Http(e) => {
                // Check common error cases that might be resolved by retry
                is_connection_refused(e) ||
                    e.is_timeout() ||
                    e.is_connect() ||
                    e.status().is_some_and(|s| s.is_server_error())
            }
            _ => false,
        }
    }
}

/// Client configuration options.
#[derive(Debug, Clone)]
pub struct TaikoPreconfClientConfig {
    /// Request timeout.
    pub timeout: Duration,
    /// Number of retry attempts.
    pub retry_attempts: usize,
    /// Base delay for exponential backoff.
    pub base_delay: Duration,
}

impl Default for TaikoPreconfClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            retry_attempts: 3,
            base_delay: Duration::from_millis(100),
        }
    }
}

/// Client that can interact with Taiko-client's preconf API.
#[derive(Debug, Clone)]
pub struct TaikoPreconfClient {
    inner: Client,
    jwt: JwtSecret,
    http_url: Url,
    ws_url: Url,
    config: TaikoPreconfClientConfig,
}

impl TaikoPreconfClient {
    /// Create a new `TaikoPreconfClient` instance.
    pub fn new(http_url: Url, ws_url: Url, jwt: JwtSecret) -> Self {
        let config = TaikoPreconfClientConfig::default();
        Self { inner: Client::new(), http_url, ws_url, jwt, config }
    }

    /// Helper function to implement exponential backoff retry logic
    async fn retry<T, F, Fut>(&self, operation: F) -> Result<T, TaikoPreconfClientError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, TaikoPreconfClientError>>,
    {
        let mut delay = self.config.base_delay;
        let mut last_err: Option<TaikoPreconfClientError> = None;

        for attempt in 0..=self.config.retry_attempts {
            if attempt > 0 {
                debug!("Retry attempt {}: sleeping for {:?}", attempt, delay);
                sleep(delay).await;
                delay = std::cmp::min(delay * 2, Duration::from_secs(10));
            }
            match operation().await {
                Ok(result) => return Ok(result),
                Err(e) if !e.is_retryable() => return Err(e),
                Err(e) => {
                    warn!("Attempt {} failed: {}.", attempt, e);
                    last_err = Some(e);
                    if attempt == self.config.retry_attempts {
                        break;
                    }
                }
            }
        }
        Err(TaikoPreconfClientError::RetriesExhausted { last_error: Box::new(last_err.unwrap()) })
    }

    /// Check the health of the preconf API.
    pub async fn health_check(&self) -> Result<(), TaikoPreconfClientError> {
        self.retry(|| async {
            let _ =
                self.inner.get(self.http_url.join("/healthz")?).send().await?.error_for_status()?;
            Ok(())
        })
        .await
    }

    /// Build a preconf block.
    pub async fn build_preconf_block(
        &self,
        body: &BuildPreconfBlockRequestBody,
    ) -> Result<Header, TaikoPreconfClientError> {
        let timestamp = current_timestamp_seconds();
        let claims = Claims { iat: timestamp, exp: Some(timestamp + 3600) };

        let token =
            self.jwt.encode(&claims).map_err(|e| TaikoPreconfClientError::Jwt(e.to_string()))?;

        // Retries have been removed temporarily so that we can deserialize errors.
        let value = self
            .inner
            .post(self.http_url.join("/preconfBlocks")?)
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, &format!("Bearer {}", token))
            .json(body)
            .send()
            .await?
            .json::<Value>()
            .await?;

        let response = serde_json::from_value::<BuildPreconfBlockResponseBody>(value.clone())
            .map_err(|e| {
                error!(?value, "Failed to deserialize /preconfBlocks response: {}", e);
                TaikoPreconfClientError::Serde(e)
            })?;

        match response {
            BuildPreconfBlockResponseBody::Success { block_header } => Ok(block_header),
            BuildPreconfBlockResponseBody::Error { error } => {
                Err(TaikoPreconfClientError::Build(error))
            }
        }
    }

    /// Get the status of the preconf block server.
    pub async fn get_status(&self) -> Result<TaikoPreconfStatus, TaikoPreconfClientError> {
        let url = self.http_url.join("/status")?;
        let res = self.inner.get(url).send().await?.error_for_status()?;
        let status = res.json::<TaikoPreconfStatus>().await?;
        Ok(status)
    }

    /// Connect to the preconf block server's WebSocket endpoint and stream
    /// the events to the caller.
    pub fn subscribe_ws_events(&self) -> ReceiverStream<TaikoPreconfWsEvent> {
        let ws_url = self.ws_url.join("/ws").unwrap_or_else(|_| self.ws_url.clone()).to_string();

        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            loop {
                let mut ws_stream = match connect_async(&ws_url).await {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        error!("Failed to connect to Taiko preconf WebSocket: {}", e);
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        continue;
                    }
                };

                while let Some(msg) = ws_stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            match serde_json::from_str::<TaikoPreconfWsEvent>(&text) {
                                Ok(event) => {
                                    if tx.try_send(event).is_err() {
                                        error!("Failed to send WebSocket event to channel");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(?e, %text, "Failed to deserialize WebSocket message")
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            debug!("WebSocket connection closed");
                            break;
                        }
                        Err(e) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        });

        ReceiverStream::new(rx)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, Bytes};
    use mk1_primitives::taiko::builder::ExecutableData;

    use super::*;

    /// Helper function to create a test request body
    const fn create_test_request_body() -> BuildPreconfBlockRequestBody {
        BuildPreconfBlockRequestBody {
            end_of_sequencing: false,
            executable_data: ExecutableData {
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                block_number: 1,
                gas_limit: 1_000_000,
                timestamp: 1_000_000,
                transactions: Bytes::new(),
                extra_data: Bytes::new(),
                base_fee_per_gas: 1_000_000,
            },
        }
    }

    #[tokio::test]
    async fn test_no_retry() {
        use mockito::Server;
        use std::{sync::mpsc, thread};
        use url::Url;

        // Create channel for server URL and mock
        let (server_tx, server_rx) = mpsc::channel();

        let server_handle = thread::spawn(move || {
            let mut server = Server::new();
            let fail_mock = server.mock("GET", "/healthz").with_status(500).expect(1).create();

            server_tx.send((server.url(), fail_mock)).unwrap();
            thread::park();
        });

        // Receive server URL and mock
        let (server_url, fail_mock) = server_rx.recv().unwrap();
        let mut client = TaikoPreconfClient::new(
            Url::parse(&server_url).unwrap(),
            Url::parse(&server_url).unwrap(),
            JwtSecret::random(),
        );
        client.config.retry_attempts = 0;

        let result = client.health_check().await;

        fail_mock.assert();
        assert!(result.is_err());

        server_handle.thread().unpark();
        server_handle.join().unwrap();
    }

    #[tokio::test]
    async fn test_retry_logic_first_fails_second_succeeds() {
        use mockito::Server;
        use std::{sync::mpsc, thread};
        use tokio::time::Instant;

        // Create channel for server URL and mocks
        let (server_tx, server_rx) = mpsc::channel();

        let server_handle = thread::spawn(move || {
            let mut server = Server::new();

            // Save mocks to verify them
            let fail_mock = server.mock("GET", "/healthz").with_status(500).expect(1).create();
            let success_mock =
                server.mock("GET", "/healthz").with_status(200).with_body("OK").expect(1).create();

            // Send server URL and mocks back
            server_tx.send((server.url(), fail_mock, success_mock)).unwrap();
            thread::park();
        });

        // Receive server URL and mocks
        let (server_url, fail_mock, success_mock) = server_rx.recv().unwrap();
        let mut client = TaikoPreconfClient::new(
            Url::parse(&server_url).unwrap(),
            Url::parse(&server_url).unwrap(),
            JwtSecret::random(),
        );

        // Only retry once
        client.config.retry_attempts = 1;

        let start_time = Instant::now();
        let result = client.health_check().await;
        let elapsed = start_time.elapsed();

        // Verify that both fail and success mocks were called
        success_mock.assert();
        fail_mock.assert();

        assert!(result.is_ok(), "Expected health_check to succeed on second try");
        assert!(
            elapsed >= client.config.base_delay,
            "Retry delay was less than expected minimum: {:?}",
            elapsed
        );

        server_handle.thread().unpark();
        server_handle.join().unwrap();
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        use mockito::Server;
        use std::thread;

        // Set up a mock server that always fails (HTTP 500) for /healthz.
        let (server_tx, server_rx) = std::sync::mpsc::channel();
        let server_handle = thread::spawn(move || {
            let mut server = Server::new();
            // Expect two calls: initial + one retry.
            let fail_mock = server.mock("GET", "/healthz").with_status(500).expect(2).create();

            server_tx.send((server.url(), fail_mock)).unwrap();
            thread::park();
        });

        // Retrieve the server URL and mock.
        let (server_url, fail_mock) = server_rx.recv().unwrap();
        let mut client = TaikoPreconfClient::new(
            Url::parse(&server_url).unwrap(),
            Url::parse(&server_url).unwrap(),
            JwtSecret::random(),
        );
        // Allow one retry (i.e. 2 attempts total).
        client.config.retry_attempts = 1;

        let result = client.health_check().await;
        fail_mock.assert();

        match result {
            Err(TaikoPreconfClientError::RetriesExhausted { last_error }) => {
                if let TaikoPreconfClientError::Http(e) = last_error.as_ref() {
                    assert_eq!(e.status(), Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
                } else {
                    panic!("Expected inner HTTP error, got a different error");
                }
            }
            _ => panic!("Expected RetriesExhausted error"),
        }

        server_handle.thread().unpark();
        server_handle.join().unwrap();
    }

    #[tokio::test]
    #[ignore] // TODO: fix this test
    async fn test_build_preconf_block_retry_logic() {
        use mockito::Server;
        use std::{sync::mpsc, thread};
        use tokio::time::Instant;

        let (server_tx, server_rx) = mpsc::channel();

        let server_handle = thread::spawn(move || {
            let mut server = Server::new();

            // Set up the fail mock with proper content-type expectations
            let fail_mock = server
                .mock("POST", "/preconfBlocks")
                .with_status(500)
                .with_header("content-type", "application/json")
                .match_header("content-type", "application/json") // Add this
                .expect(1)
                .create();

            // Set up the success mock with proper content-type expectations
            let success_mock = server
                .mock("POST", "/preconfBlocks")
                .with_status(200)
                .with_header("content-type", "application/json")
                .match_header("content-type", "application/json")
                .with_body(serde_json::json!({
                    "blockHeader": {
                        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
                        "beneficiary": "0x0000000000000000000000000000000000000000",
                        "stateRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
                        "transactionsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                        "receiptsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                        "logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                        "difficulty": "0x0",
                        "number": "0x1",
                        "gasLimit": "0x1c9c380",
                        "gasUsed": "0x0",
                        "timestamp": "0x0",
                        "extraData": "0x",
                        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                        "nonce": "0x0000000000000000",
                        "baseFeePerGas": "0x3b9aca00",
                        "withdrawalsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                        "blobGasUsed": "0x0",
                        "excessBlobGas": "0x0",
                        "hash": "0x0000000000000000000000000000000000000000000000000000000000000000"
                    }
                }).to_string())
                .expect(1)
                .create();

            server_tx.send((server.url(), fail_mock, success_mock)).unwrap();
            thread::park();
        });

        let (server_url, fail_mock, success_mock) = server_rx.recv().unwrap();
        let mut client = TaikoPreconfClient::new(
            Url::parse(&server_url).unwrap(),
            Url::parse(&server_url).unwrap(),
            JwtSecret::random(),
        );
        client.config.retry_attempts = 1;

        let request_body = create_test_request_body();

        let start_time = Instant::now();
        let result = client.build_preconf_block(&request_body).await;
        let elapsed = start_time.elapsed();

        // Verify both mocks were called
        fail_mock.assert();
        success_mock.assert();

        assert!(result.is_ok(), "Expected build_preconf_block to succeed on second try");
        assert!(
            elapsed >= client.config.base_delay,
            "Retry delay was less than expected minimum: {:?}",
            elapsed
        );

        // Verify the response structure
        let response = result.unwrap();
        assert_eq!(response.number, 1);
        assert_eq!(
            response.hash.to_string(),
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        );

        server_handle.thread().unpark();
        server_handle.join().unwrap();
    }

    #[tokio::test]
    #[ignore] // TODO: fix this test
    async fn test_non_retryable_error() {
        use mockito::Server;
        use std::{sync::mpsc, thread};

        let (server_tx, server_rx) = mpsc::channel();

        let server_handle = thread::spawn(move || {
            let mut server = Server::new();

            // Create a mock that returns 400 Bad Request (non-retryable)
            let bad_request_mock = server
                .mock("POST", "/preconfBlocks")
                .with_status(400)
                .with_header("content-type", "application/json")
                .match_header("content-type", "application/json")
                .expect(1) // Should only be called once since it's non-retryable
                .create();

            server_tx.send((server.url(), bad_request_mock)).unwrap();
            thread::park();
        });

        let (server_url, bad_request_mock) = server_rx.recv().unwrap();
        let mut client = TaikoPreconfClient::new(
            Url::parse(&server_url).unwrap(),
            Url::parse(&server_url).unwrap(),
            JwtSecret::random(),
        );

        // Set up multiple retry attempts to verify it doesn't retry
        client.config.retry_attempts = 3;
        client.config.base_delay = Duration::from_millis(1);

        let request_body = create_test_request_body();

        let result = client.build_preconf_block(&request_body).await;

        // Verify the mock was called exactly once
        bad_request_mock.assert();

        // Verify that we got an error
        assert!(result.is_err());

        // Verify it was a non-retryable HTTP error
        match result.unwrap_err() {
            TaikoPreconfClientError::Http(e) => {
                assert_eq!(e.status(), Some(reqwest::StatusCode::BAD_REQUEST));
            }
            _ => panic!("Expected Http error"),
        }

        server_handle.thread().unpark();
        server_handle.join().unwrap();
    }
}
