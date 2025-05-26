use alloy::consensus::{EthereumTxEnvelope, Transaction, TxEip4844Variant};

/// A trait for objects that can be summarized into a string for logging purposes.
///
/// Sometimes the Debug impl is too verbose, and the Display impl does something different
/// than what we want. This trait allows us to have our custom verbosity.
pub trait Summary {
    /// Returns a summary of the object.
    fn summary(&self) -> String;
}

impl Summary for EthereumTxEnvelope<TxEip4844Variant> {
    fn summary(&self) -> String {
        format!(
            "chain_id={}, nonce={}, gas_limit={}, max_fee_per_gas={}, max_priority_fee_per_gas={}, to={}, value={}, blob_versioned_hashes={:?}, max_fee_per_blob_gas={}, input_size={}, blob_count={}, hash={}",
            self.chain_id().unwrap_or_default(),
            self.nonce(),
            self.gas_limit(),
            self.max_fee_per_gas(),
            self.max_priority_fee_per_gas().unwrap_or_default(),
            self.to().unwrap_or_default(),
            self.value(),
            self.blob_versioned_hashes().unwrap_or_default(),
            self.max_fee_per_blob_gas().unwrap_or_default(),
            self.input().len(),
            self.blob_versioned_hashes().unwrap_or_default().len(),
            self.hash()
        )
    }
}
