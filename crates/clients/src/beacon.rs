use std::fmt::Debug;

use derive_more::derive::{Deref, From};
use url::Url;

/// Ethereum Beacon client connection
#[derive(Clone, Deref, From)]
pub struct BeaconClient {
    inner: beacon_api_client::mainnet::Client,
}

impl BeaconClient {
    /// Create a new `BeaconClient` instance.
    pub fn new(url: Url) -> Self {
        let inner = beacon_api_client::Client::new(url);
        Self { inner }
    }

    /// Check if the beacon client is responsive and healthy.
    /// Returns `Ok(())` if the client is responsive and healthy, or an error if not.
    pub async fn health_check(&self) -> Result<(), beacon_api_client::Error> {
        // Use the dedicated health check endpoint
        self.inner.get_health().await.map(|_| ())
    }
}

impl Debug for BeaconClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BeaconClient").finish()
    }
}
