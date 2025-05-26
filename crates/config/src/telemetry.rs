use std::{collections::HashMap, net::SocketAddr, time::Duration};

use clap::Parser;
use metrics_exporter_prometheus::{BuildError, PrometheusBuilder};
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// The OpenTelemetry logs endpoint for Axiom.
const AXIOM_LOGS_API: &str = "https://api.axiom.co/v1/logs";

/// Telemetry-related configuration options
#[derive(Debug, Clone, Parser)]
pub struct TelemetryOpts {
    /// Whether to use ANSI colors in the logs. Disable if you're piping logs to a file or using
    /// third party services to collect logs, like kubectl/cloudwatch/loki etc.
    #[clap(long = "telemetry.use-ansi", env = "MK1_TELEMETRY_USE_ANSI", default_value_t = true)]
    pub use_ansi: bool,
    /// The port to listen for Prometheus metrics. Default is `9090`.
    #[clap(long = "metrics.port", env = "MK1_METRICS_PORT", default_value_t = 9090)]
    pub metrics_port: u16,
    /// Disable metrics collection. Default is `false`.
    #[clap(long = "metrics.disable", env = "MK1_DISABLE_METRICS", default_value_t = false)]
    pub disable_metrics: bool,
}

/// A wrapper around the OpenTelemetry logger provider.
#[derive(Debug, Default)]
pub struct LogProvider {
    inner: Option<SdkLoggerProvider>,
}

impl LogProvider {
    /// Set the OpenTelemetry logger provider.
    pub fn set_provider(&mut self, provider: SdkLoggerProvider) {
        self.inner = Some(provider);
    }

    /// Shutdown the OpenTelemetry logger provider.
    pub fn shutdown(&self) {
        if let Some(provider) = self.inner.as_ref() {
            // We ignore the error because it's not critical
            let _ = provider.shutdown();
        }
    }
}

impl TelemetryOpts {
    /// Setup the telemetry stack for Mk1.
    ///
    /// 1. OpenTelemetry tracer provider with tracing to stdout (and optionally to Axiom)
    /// 2. Metrics collection with Prometheus (if enabled)
    pub fn setup(&self, instance_name: &str) -> Result<LogProvider, BuildError> {
        let mut global_provider = LogProvider::default();
        // Setup tracing with stdout by default
        let registry = tracing_subscriber::registry()
            .with(EnvFilter::from_env("RUST_LOG"))
            .with(tracing_subscriber::fmt::layer().with_ansi(self.use_ansi));

        // Try to add Axiom logging for server environments
        if let Some(provider) = build_axiom_provider(instance_name) {
            let layer = OpenTelemetryTracingBridge::new(&provider);
            global_provider.set_provider(provider);
            registry.with(layer).init();
            info!("Axiom logging enabled");
        } else {
            registry.init();
        }

        // Setup metrics collection with Prometheus
        if !self.disable_metrics {
            let prometheus_address = SocketAddr::from(([0, 0, 0, 0], self.metrics_port));

            PrometheusBuilder::new()
                .with_http_listener(prometheus_address)
                .add_global_label("instance", instance_name)
                .install()?;

            info!("Metrics enabled on {}", prometheus_address);
        }

        Ok(global_provider)
    }
}

/// Builds the Axiom log provider if the `AXIOM_TOKEN` and `AXIOM_DATASET` environment variables
/// are set.
fn build_axiom_provider(name: &str) -> Option<SdkLoggerProvider> {
    let Ok(token) = std::env::var("AXIOM_TOKEN") else {
        return None;
    };

    let Ok(dataset) = std::env::var("AXIOM_DATASET") else {
        return None;
    };

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_owned(), format!("Bearer {}", token));
    headers.insert("X-Axiom-Dataset".to_owned(), dataset);

    let exporter = LogExporter::builder()
        .with_http()
        .with_headers(headers)
        .with_endpoint(AXIOM_LOGS_API)
        .with_timeout(Duration::from_secs(5))
        .build()
        .expect("Failed to build Axiom log exporter");

    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                // OTLP convention
                .with_attribute(KeyValue::new("service.name", name.to_owned()))
                .build(),
        )
        .build();

    Some(provider)
}
