#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Various extensible API client connections

/// Beacon chain client connection
pub mod beacon;

/// `AuthRPC` (engine API) connection
pub mod engine;

/// Execution layer client connection
pub mod execution;

/// Taiko Preconf client API connection
pub mod taiko_preconf;
