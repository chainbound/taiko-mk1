#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Mk1 sequencer driver
//!
//! The driver is responsible for:
//! - Producing new L2 blocks when it is the active sequencer
//! - Proposing batches to L1 at the end of its sequencing window

/// The main driver module with the core event loop.
mod driver;
pub use driver::Driver;

/// The lookahead is the range of slots that the driver is allowed to include and sequence.
/// It is calculated based on the current and next epoch's preconfirmer in the
/// [`TaikoPreconfWhitelist`] contract.
mod lookahead;

/// The driver configuration.
mod config;

/// The block builder, responsible for building and finalizing new L2 blocks.
mod block_builder;

/// The batch submitter, responsible for submitting batches to L1.
mod batch_submitter;

/// Helper functions.
mod helpers;

/// The metrics for the driver.
mod metrics;

/// The sync module, responsible for syncing the driver on startup and
/// maintaining an updated state when starting a new inclusion window.
mod sync;

/// The driver state containers.
mod state;

/// The driver status.
mod status;
