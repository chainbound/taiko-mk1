use alloy::primitives::Address;
use alloy_rpc_types_engine::JwtSecret;
use clap::Parser;
use url::Url;

/// L1-related configuration options
#[derive(Debug, Clone, Parser)]
pub struct L1Opts {
    /// The URL of the L1 execution client HTTP connection
    #[clap(long = "l1.el-url", env = "MK1_L1_EXECUTION_URL", id = "l1-el-url")]
    pub el_url: Url,
    /// The URL of the L1 execution client WebSocket connection
    #[clap(long = "l1.el-ws-url", env = "MK1_L1_EXECUTION_WS_URL", id = "l1-el-ws-url")]
    pub el_ws_url: Url,
    /// The URL of the L1 consensus client HTTP connection
    #[clap(long = "l1.cl-url", env = "MK1_L1_CONSENSUS_URL")]
    pub cl_url: Url,
}

/// L2-related configuration options
#[derive(Debug, Clone, Parser)]
pub struct L2Opts {
    /// The URL of the L2 execution client (taiko-geth) HTTP connection
    #[clap(long = "l2.el-url", env = "MK1_L2_EXECUTION_URL", id = "l2-el-url")]
    pub el_url: Url,
    /// The URL of the L2 execution client (taiko-geth) WebSocket connection
    #[clap(long = "l2.el-ws-url", env = "MK1_L2_EXECUTION_WS_URL", id = "l2-el-ws-url")]
    pub el_ws_url: Url,
    /// The URL of the L2 engine client (taiko-geth) HTTP connection
    #[clap(long = "l2.engine-url", env = "MK1_L2_ENGINE_URL")]
    pub engine_url: Url,
    /// The JWT secret to communicate with the L2 engine client
    #[clap(long = "l2.jwt-secret", env = "MK1_L2_JWT_SECRET")]
    pub jwt_secret: JwtSecret,
    /// The URL of the Taiko L2 Preconf API (taiko-client) HTTP connection
    #[clap(long = "l2.preconf-url", env = "MK1_L2_PRECONF_URL")]
    pub preconf_url: Url,
    /// The URL of the Taiko L2 Preconf API (taiko-client) WebSocket connection
    #[clap(long = "l2.preconf-ws-url", env = "MK1_L2_PRECONF_WS_URL")]
    pub preconf_ws_url: Url,
}

/// The configuration for the chain.
#[derive(Debug, Clone, Parser)]
pub struct ChainOpts {
    /// The L2 block time (in seconds).
    #[clap(long = "chain.l2-block-time", env = "MK1_L2_BLOCK_TIME")]
    pub l2_block_time: u64,
    /// The L1 block time (in seconds).
    #[clap(long = "chain.l1-block-time", env = "MK1_L1_BLOCK_TIME")]
    pub l1_block_time: u64,
}

/// The contract addresses required to run the sequencer.
#[derive(Debug, Clone, Parser)]
pub struct ContractAddresses {
    /// The address of the L1 `TaikoInbox.sol`
    #[clap(long = "contracts.taiko-inbox", env = "MK1_TAIKO_INBOX")]
    pub taiko_inbox: Address,
    /// The address of the L1 `PreconfWhitelist.sol`
    #[clap(long = "contracts.preconf-whitelist", env = "MK1_PRECONF_WHITELIST")]
    pub preconf_whitelist: Address,
    /// The address of the L1 `PreconfRouter.sol`
    #[clap(long = "contracts.preconf-router", env = "MK1_PRECONF_ROUTER")]
    pub preconf_router: Address,
    /// The address of the L1 `TaikoToken.sol`
    #[clap(long = "contracts.taiko-token", env = "MK1_TAIKO_TOKEN")]
    pub taiko_token: Address,
    /// The address of the L1 `TaikoWrapper.sol`
    #[clap(long = "contracts.taiko-wrapper", env = "MK1_TAIKO_WRAPPER")]
    pub taiko_wrapper: Address,

    /// The address of the L2 `TaikoAnchor.sol`
    #[clap(long = "contracts.taiko-anchor", env = "MK1_TAIKO_ANCHOR")]
    pub taiko_anchor: Address,
}
