use alloy_primitives::{Address, B256, address, b256};

/// The address of the "golden touch" account, used to sign the `anchor` L2 transactions.
///
/// Ref: <https://github.com/taikoxyz/taiko-mono/blob/c7e9bfbd65f3d7515b66933d96a5c910ab00f82a/packages/protocol/contracts/layer2/based/TaikoAnchor.sol#L31>
pub const TAIKO_GOLDEN_TOUCH_ACCOUNT: Address =
    address!("0000777735367b36bC9B61C50022d9D0700dB4Ec");

/// The signer of the "golden touch" account, used to sign the `anchor` L2 transactions.
///
/// It corresponds to the address: `0x0000777735367b36bC9B61C50022d9D0700dB4Ec`.
///
/// Ref: <https://github.com/taikoxyz/taiko-mono/blob/c7e9bfbd65f3d7515b66933d96a5c910ab00f82a/packages/taiko-client/bindings/encoding/struct.go#L23>
pub const TAIKO_GOLDEN_TOUCH_PRIVATE_KEY: B256 =
    b256!("92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38");

/// The gas limit for the `anchorV3` L2 transactions.
///
/// Ref: <https://github.com/taikoxyz/taiko-geth/blob/taiko/consensus/taiko/consensus.go#L43>
pub const TAIKO_ANCHOR_V3_GAS_LIMIT: u64 = 1_000_000;

/// The maximum timeshift in seconds allowed between any 2 blocks in an outstanding batch.
/// This is necessary because timeshifts are cast to `u8` in `TaikoInbox.sol`.
pub const MAX_TIMESHIFT_SECS: u64 = u8::MAX as u64;

/// The buffer to account for the maximum timeshift in seconds allowed between any 2 blocks in an
/// outstanding batch. This should only be used if you need the safety margin, such as when
/// producing empty blocks to reset the timeshift.
pub const TIMESHIFT_BUFFER_SECS: u64 = MAX_TIMESHIFT_SECS - 4;
