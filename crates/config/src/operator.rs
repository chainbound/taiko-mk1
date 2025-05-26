use alloy::{consensus::constants::ETH_TO_WEI, primitives::U256, signers::local::PrivateKeySigner};
use clap::Parser;

/// Operator-related configuration options
#[derive(Debug, Clone, Parser)]
pub struct OperatorOpts {
    /// The private key of the Taiko preconfirmer, also called "operator"
    #[clap(long = "operator.private-key", env = "MK1_OPERATOR_PRIVATE_KEY")]
    pub private_key: PrivateKeySigner,
    /// The minimum ETH balance required to run the sequencer (in wei)
    ///
    /// Default: 1 ETH
    #[clap(long = "operator.min-eth", env = "MK1_MIN_ETH_BALANCE", default_value_t = U256::from(ETH_TO_WEI))]
    pub min_eth_balance: U256,
    /// The minimum Taiko token balance required to run the sequencer (in wei)
    ///
    /// Default: 1000 TAIKO
    #[clap(long = "operator.min-taiko", env = "MK1_MIN_TAIKO_BALANCE", default_value_t = U256::from(1000 * ETH_TO_WEI))]
    pub min_taiko_balance: U256,
}
