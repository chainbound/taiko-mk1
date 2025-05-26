#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

//! Mk1 sequencer primitive types, utilities and constants.

use std::ops::Range;

use alloy::eips::{eip4895::GWEI_TO_WEI, merge::EPOCH_SLOTS};
use alloy_primitives::{Address, U256};
use serde::Serializer;

/// Taiko chain primitives and constants.
pub mod taiko;

/// Blob encoding utilities.
pub mod blob;

/// Time-related utilities.
pub mod time;

/// Compression utilities.
pub mod compression;

/// Trasport retries utilities
pub mod retries;

/// Utility for summarizing objects into a string for logging purposes.
pub mod summary;

/// Utilities for triggering shutdown signals from active tasks.
pub mod shutdown;

/// Utilities for handling long-running tasks.
pub mod task;

/// An L1 slot number alias.
pub type Slot = u64;
/// Convert a slot number to an epoch number.
pub trait SlotUtils: Sized {
    /// Convert a slot number to an epoch number.
    fn to_epoch(self) -> Epoch;

    /// Return the slot at the beginning of the epoch.
    fn beginning_of_epoch(self) -> Slot {
        self.to_epoch().to_slot()
    }
}

impl SlotUtils for Slot {
    fn to_epoch(self) -> Epoch {
        self / EPOCH_SLOTS
    }
}

/// An L1 epoch number alias.
pub type Epoch = u64;
/// Convert an epoch number to a slot number.
pub trait EpochUtils: Sized {
    /// Convert an epoch number to a slot number.
    fn to_slot(self) -> Slot;

    /// Return the range of slots for the epoch.
    fn to_slot_range(self) -> Range<Slot> {
        let start = self.to_slot();
        let end = start.saturating_add(EPOCH_SLOTS);
        start..end
    }
}

impl EpochUtils for Epoch {
    fn to_slot(self) -> Slot {
        self * EPOCH_SLOTS
    }
}

/// A constant for the number of bytes in a kilobyte.
pub const BYTES_PER_KB: usize = 1024;

/// Serialize an address to a string (hex-encoded and checksummed).
///
/// Reference: <https://github.com/alloy-rs/core/pull/765>
pub fn serialize_address_to_string<S>(address: &Address, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&address.to_string())
}

/// From a balance expressed in wei, return the balance in ETH as a f64 with gwei precision.
///
/// NOTE:
/// * returns zero if the balance is less than 1 gwei;
/// * returns at most a balance of ~9M ETH to avoid floating point inaccuracies above certain
///   numbers.
pub fn wei_to_eth(balance: U256) -> f64 {
    const GWEI_IN_ETH: f64 = 1e-9;
    const F64_REPRESENTATION_THRESHOLD: u64 = 1 << 53; // 2^53

    let balance_in_gwei = balance / U256::from(GWEI_TO_WEI);

    if balance_in_gwei.is_zero() {
        return 0.0;
    }

    if balance_in_gwei > U256::from(F64_REPRESENTATION_THRESHOLD) {
        // Directly return 2^53 * [`GWEI_IN_ETH`]
        return 9_007_199.254740992;
    }

    // Now, we know that the first limb of the balance fits in a f64 with correct precision.
    balance_in_gwei.as_limbs()[0] as f64 * GWEI_IN_ETH
}

#[cfg(test)]
mod tests {
    use alloy::consensus::constants::ETH_TO_WEI;

    use super::*;

    #[test]
    fn wei_to_eth_small() {
        let balance = U256::from(1_000);
        let balance_eth = wei_to_eth(balance);
        assert_eq!(balance_eth, 0.0, "Balance should be zero. Got: {balance_eth}");
    }

    #[test]
    fn wei_to_eth_one_gwei() {
        let balance = U256::from(GWEI_TO_WEI);
        let balance_eth = wei_to_eth(balance);
        let expected = 1e-9;
        assert_eq!(expected, balance_eth, "Expected balance: {expected}. Got: {balance_eth}");
    }

    #[test]
    fn wei_to_eth_half_eth() {
        let balance = U256::from(ETH_TO_WEI / 2);
        let balance_eth = wei_to_eth(balance);
        let expected = 0.5;
        assert_eq!(expected, balance_eth, "Expected balance: {expected}. Got: {balance_eth}");
    }

    #[test]
    fn wei_to_eth_above_ten_million_eth_is_capped() {
        let original_balance_eth = 10_000_000;
        let balance = U256::from(ETH_TO_WEI * original_balance_eth);
        let balance_eth = wei_to_eth(balance);
        assert!(
            (original_balance_eth as f64) - balance_eth < 1_000_000_f64,
            "Computed balance should be less than the original balance and capped"
        );
    }
}
