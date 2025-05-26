#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Preconfirmation Sequencer for Taiko Alethia chains.
//!
//! Authors: Chainbound Developers <dev@chainbound.io>

use clap::Parser;
use tracing::info;

use mk1_config::Opts;
use mk1_driver::Driver;
use mk1_primitives::shutdown::{ShutdownSignal, run_until_shutdown};

mod allocator;
use allocator::{Allocator, new_allocator};

#[global_allocator]
static ALLOC: Allocator = new_allocator();

#[tokio::main]
async fn main() -> eyre::Result<()> {
    if let Ok(custom_env_file) = std::env::var("ENV_FILE") {
        // Try from custom env file, and abort if it fails
        dotenvy::from_filename(custom_env_file)?;
    } else {
        // Try from default .env file, and ignore if it fails. It might
        // be that the user isn't using it.
        dotenvy::dotenv().ok();
    }

    let opts = Opts::parse();

    let tracer_provider = opts.telemetry.setup(&opts.instance_name)?;

    info!("👨‍🚀 MK1 engine starting...");

    let shutdown_signal = ShutdownSignal::new();
    let on_shutdown = || {
        info!("👋 MK1 engine shutting down...");
        tracer_provider.shutdown();
    };

    let run_driver = async { Driver::new(opts).await?.startup_sync().await.start().await };

    run_until_shutdown(run_driver, shutdown_signal, on_shutdown).await
}
