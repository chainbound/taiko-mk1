use alloy::{
    contract::Error as ContractError,
    primitives::{B256, BlockNumber, Bytes},
    providers::SendableTxErr,
    rpc::types::TransactionRequest,
    transports::{RpcError, TransportErrorKind},
};
use alloy_sol_types::Error as SolError;

use mk1_chainio::{TryParseTransportErrorResult, taiko::inbox::ITaikoInbox::ITaikoInboxErrors};
use mk1_primitives::blob::BlobError;
use thiserror::Error;
use tracing::error;

/// The error message that indicates the blob gas is too low.
/// This is a retryable error.
pub(crate) const BLOB_GAS_TOO_LOW_ERROR: &str = "max fee per blob gas less than block blob gas fee";

/// The error message that indicates the maximum priority fee gas per gas is too low.
/// This is a retryable error.
pub(crate) const MAX_FEE_PER_GAS_TOO_LOW_ERROR: &str = "max fee per gas less than block base fee";

/// The error message that indicates the maximum fee gas per gas is too low.
pub(crate) const NONCE_TOO_LOW_ERROR: &str = "nonce too low";

/// Error that can occur while creating or proposing a batch.
#[derive(Debug, thiserror::Error)]
pub enum BatchProposalError {
    #[error("Error while compressing batch transactions with Zlib")]
    Zlib(#[from] std::io::Error),
    #[error("Error while creating blob sidecar from batch data")]
    BlobSidecar(#[from] BlobError),
    #[error(transparent)]
    RpcError(#[from] RpcError<TransportErrorKind>),
    #[error("Deadline reached, no slots left for including batch")]
    DeadlineReached,
    #[error("Failed to convert tx request into tx envelope: {0}")]
    FailedToConvertIntoTxEnvelop(#[from] SendableTxErr<TransactionRequest>),
    #[error("Failed to parse contract error: {0}")]
    FailedToParseContractError(#[from] ContractError),
    #[error("Failed to propose batch: {0}")]
    SolidityError(#[from] SolError),
    #[error(transparent)]
    BatchRetryError(#[from] RetryBatchSubmissionError),
    #[error("Latest L1 head unavailable")]
    NoHead,
    #[error("Batch reverted. Hash: {0}, block number: {1:?}")]
    Reverted(B256, Option<BlockNumber>),
}

impl BatchProposalError {
    /// Returns `true` if the error a `NotPreconfer` error.
    pub(crate) const fn matches_not_preconfer(&self) -> bool {
        if let Self::BatchRetryError(e) = self { e.matches_not_preconfer() } else { false }
    }

    pub(crate) fn matches_nonce_too_low(&self) -> bool {
        if let Self::BatchRetryError(RetryBatchSubmissionError::RpcError(RpcError::ErrorResp(e))) =
            self
        {
            let msg = e.message.to_lowercase();
            msg.contains(NONCE_TOO_LOW_ERROR)
        } else {
            false
        }
    }
}

/// Error that can occur while retrying batch submission.
#[derive(Debug, Error)]
pub enum RetryBatchSubmissionError {
    #[error("RPC error while interacting with contract: {0}")]
    RpcError(#[from] RpcError<TransportErrorKind>),
    #[error("TaikoInbox error: {0:?}")]
    TaikoInbox(ITaikoInboxErrors),
    #[error("Execution reverted with unknown selector")]
    UnknownSelector(Bytes),
    #[error("Transaction fees too high. Got {0}, max {1}")]
    FeesTooHigh(u128, u128),
}

impl RetryBatchSubmissionError {
    /// Returns `true` if is a `NotPreconfer` or `NotTheOperator` error.
    pub(crate) const fn matches_not_preconfer(&self) -> bool {
        matches!(self, Self::TaikoInbox(ITaikoInboxErrors::NotPreconfer(_))) ||
            matches!(self, Self::TaikoInbox(ITaikoInboxErrors::NotTheOperator(_)))
    }
}

impl From<TryParseTransportErrorResult<ITaikoInboxErrors>> for RetryBatchSubmissionError {
    fn from(result: TryParseTransportErrorResult<ITaikoInboxErrors>) -> Self {
        match result {
            TryParseTransportErrorResult::UnknownSelector(s) => Self::UnknownSelector(s),
            TryParseTransportErrorResult::Original(e) => Self::RpcError(e),
            TryParseTransportErrorResult::Decoded(e) => Self::TaikoInbox(e),
        }
    }
}

/// Extracts the base fee from the error message like [`MAX_FEE_PER_GAS_TOO_LOW`].
///
/// Example full err: "failed with 35894639 gas: max fee per gas less than block base fee: address
/// 0xA5a9D8524077714378E12aF057B40d42AAd79F3C, maxFeePerGas: 1009953, baseFee: 64546698"
///
/// References:
/// * <https://github.com/ethereum/go-ethereum/blob/bca0646ede39d45303d8bd0b24ff5e7efa4f3e28/eth/gasestimator/gasestimator.go#L256>
/// * <https://github.com/ethereum/go-ethereum/blob/bca0646ede39d45303d8bd0b24ff5e7efa4f3e28/core/state_transition.go#L349-L350>
///
/// Returns `None` upon ANY error, including if the base fee is not present in the error message.
pub(crate) fn extract_base_fee_from_err(err: &str) -> Option<u128> {
    let err = err.trim().to_lowercase();
    let prefix = "basefee: ";

    let start = err.find(prefix)?;

    let start_idx = start + prefix.len();
    let end_idx = err
        .get(start_idx..)?
        .find(|c: char| !c.is_ascii_digit())
        // if the string ends with a number, we need to use the length of the string then
        .map_or(err.len(), |end| start_idx + end);

    err.get(start_idx..end_idx)?.parse::<u128>().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn extract_base_fee_from_err_works() {
        let test_str = "failed with 35894639 gas: max fee per gas less than block base fee: address 0xA5a9D8524077714378E12aF057B40d42AAd79F3C, maxFeePerGas: 1009953, baseFee: 64546698";
        let result = super::extract_base_fee_from_err(test_str);
        assert_eq!(result, Some(64546698));
    }

    #[test]
    fn extract_base_fee_from_err_fails() {
        // Slightly different error message without "baseFee: "
        let test_str = "failed with 35894639 gas: max fee per gas less than block base fee: address 0xA5a9D8524077714378E12aF057B40d42AAd79F3C, maxFeePerGas: 1009953";
        let result = super::extract_base_fee_from_err(test_str);
        assert_eq!(result, None);
    }

    #[test]
    fn extract_base_fee_from_err_nil_base_fee_fails() {
        // Slightly different error message without "baseFee: "
        let test_str = "failed with 35894639 gas: max fee per gas less than block base fee: address 0xA5a9D8524077714378E12aF057B40d42AAd79F3C, maxFeePerGas: 1009953, baseFee: nil";
        let result = super::extract_base_fee_from_err(test_str);
        assert_eq!(result, None);
    }
}
