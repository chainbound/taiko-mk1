use std::{fmt, sync::LazyLock};

use alloy::{
    consensus::{Blob, BlobTransactionSidecar},
    eips::{
        eip1559::Eip1559Estimation,
        eip4844::{BYTES_PER_BLOB, VERSIONED_HASH_VERSION_KZG},
    },
};
use alloy_primitives::{B256, Bytes, keccak256};
use ethereum_consensus::{crypto::KzgCommitment, deneb::mainnet::BlobSidecar};
use tokio::runtime::{Builder, Runtime};

/// Gas Fee hints for producing a blob transaction.
#[derive(Debug, Clone, Copy)]
pub struct BlobTxFeesHints {
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    blob_fee_per_gas: u128,
}

impl BlobTxFeesHints {
    /// Create new gas hints.
    #[inline]
    pub const fn new(
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
        blob_fee_per_gas: u128,
    ) -> Self {
        Self { max_priority_fee_per_gas, max_fee_per_gas, blob_fee_per_gas }
    }

    /// Get the max priority fee per gas.
    #[inline]
    pub const fn max_priority_fee_per_gas(&self) -> u128 {
        self.max_priority_fee_per_gas
    }

    /// Get the max fee per gas.
    #[inline]
    pub const fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }

    /// Get the blob fee per gas.
    #[inline]
    pub const fn blob_fee_per_gas(&self) -> u128 {
        self.blob_fee_per_gas
    }

    /// Create hints from an EL estimation
    #[inline]
    pub const fn from_estimation(
        eip_1559_estimation: Eip1559Estimation,
        blob_fee_per_gas: u128,
    ) -> Self {
        Self {
            max_priority_fee_per_gas: eip_1559_estimation.max_priority_fee_per_gas,
            max_fee_per_gas: eip_1559_estimation.max_fee_per_gas,
            blob_fee_per_gas,
        }
    }

    /// Doubles all the fees in the gas hints.
    ///
    /// This is needed when trying to resubmit a blob transaction in the mempool, according
    /// to Geth's blobpool.
    ///
    /// Reference: <https://github.com/ethereum/go-ethereum/blob/b47e4d5b38b34c045cb10af6c0b5603c285310cd/core/txpool/blobpool/blobpool.go#L1142-L1179>
    #[inline]
    pub const fn double(&mut self) {
        self.max_priority_fee_per_gas *= 2;
        self.max_fee_per_gas *= 2;
        self.blob_fee_per_gas *= 2;
    }

    /// Increase base fee and blob base fee by 12.5% to account for a bump of fees
    /// in the next block.
    #[inline]
    pub const fn with_next_block_increase(self) -> Self {
        let base_fee = self.max_fee_per_gas - self.max_priority_fee_per_gas;
        let base_fee_increased = base_fee * 1_125 / 1_000 + 1;
        let max_fee_per_gas_increased = self.max_fee_per_gas - base_fee + base_fee_increased;

        let blob_base_fee_increased = self.blob_fee_per_gas * 1_125 / 1_000 + 1;
        Self {
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            max_fee_per_gas: max_fee_per_gas_increased,
            blob_fee_per_gas: blob_base_fee_increased,
        }
    }

    /// Increase base fee and blob base fee by 12.5% to account for a bump of fees
    /// in the next block.
    #[inline]
    pub const fn next_block_increase(&mut self) {
        *self = self.with_next_block_increase();
    }

    /// Makes sure that the priority fee at least the specified `tip`.
    #[inline]
    pub const fn with_enforced_min_batch_tip_wei(self, tip: u128) -> Self {
        let diff = tip.saturating_sub(self.max_priority_fee_per_gas);
        if diff > 0 {
            return Self {
                max_priority_fee_per_gas: tip + 1,
                max_fee_per_gas: self.max_fee_per_gas + diff + 1,
                blob_fee_per_gas: self.blob_fee_per_gas + 1,
            };
        }

        self
    }

    /// Increase the base fee to the provided value, ensuring the max priority is at least 1/10 of
    /// this new value.
    ///
    /// Returns `true` if the base fee was increased, `false` otherwise.
    pub const fn bump_base_fee(&mut self, new_base_fee: u128) -> bool {
        if new_base_fee < self.max_fee_per_gas.saturating_sub(self.max_priority_fee_per_gas) {
            // No-op if this is not a bump
            return false;
        }

        self.max_fee_per_gas = new_base_fee + self.max_priority_fee_per_gas;

        if self.max_priority_fee_per_gas < new_base_fee / 10 {
            // 1. Remove the previous increase
            self.max_fee_per_gas =
                self.max_fee_per_gas.saturating_sub(self.max_priority_fee_per_gas);
            // 2. Compute the new max priority fee
            self.max_priority_fee_per_gas = new_base_fee.div_ceil(10);
            // 3. Add the new max priority fee to the max fee
            self.max_fee_per_gas =
                self.max_fee_per_gas.saturating_add(self.max_priority_fee_per_gas);
        }

        true
    }
}

impl fmt::Display for BlobTxFeesHints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(max_fee={}, max_priority_fee={}, blob_fee={})",
            self.max_fee_per_gas, self.max_priority_fee_per_gas, self.blob_fee_per_gas
        )
    }
}

// Constants
const ENCODING_VERSION: u8 = 0;
const VERSION_OFFSET: usize = 1;
const ROUNDS: usize = 1024;

/// The maximum size of a blob's data, in bytes. Corresponds to:
/// - 4 bytes per field element * 31 field elements per row
/// - 3 additional bytes for correct field element alignment
/// - multiplied by 1024 rows
/// - minus 1 byte for version, and 3 bytes for length prefix
/// - This gives us 130044 bytes of usable space per blob
pub const MAX_BLOB_DATA_SIZE: usize = (4 * 31 + 3) * 1024 - 4; // (127 * 1024) - 4 = 130044

/// Define a custom thread pool with larger stack size for blob encoding.
///
/// The reason for this is that if we tried to `tokio::spawn()` a task and call
/// `BlobTransactionSidecar::try_from_blobs_bytes` inside it, the binary panics with a
/// "tokio runtime: stack overflow" error. The default stack size is 2MB which is not enough here.
static BLOB_THREAD_POOL: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .thread_name("blob-worker")
        .worker_threads(2)
        .thread_stack_size(8 * 1024 * 1024) // 8MB stack size
        .build()
        .expect("Failed to create blob worker thread pool")
});

/// An error type for blob encoding errors.
#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum BlobError {
    #[error("too much data to encode in one blob: len={0}")]
    InputTooLarge(usize),
    #[error("data did not fit in blob: read_offset={read_offset}, data_len={data_len}")]
    DataDidNotFit { read_offset: usize, data_len: usize },
    #[error("KZG error: {0}")]
    KZGError(Box<dyn std::error::Error + Sync + Send>),
    #[error("thread panicked: {0}")]
    ThreadPanicked(#[from] tokio::task::JoinError),
    #[error("invalid version byte: want={want}, got={got}")]
    InvalidVersion { want: u8, got: u8 },
    #[error("invalid length: got={got}, exceeds maximum={maximum}")]
    InvalidLength { got: usize, maximum: usize },
    #[error("extraneous data: {0}")]
    ExtraneousData(String),
    #[error("invalid field element")]
    InvalidFieldElement,
}

/// Encodes the provided input data into a list of blobs, and returns a sidecar.
///
/// This operation blocks the current thread until the encoding is complete.
pub fn create_blob_sidecar_from_data_blocking(
    data: &[u8],
) -> Result<BlobTransactionSidecar, BlobError> {
    // Split the input data into chunks of `MAX_BLOB_DATA_SIZE` and encode each chunk into a blob
    let blobs = data
        .chunks(MAX_BLOB_DATA_SIZE)
        .map(create_blob_from_data)
        .collect::<Result<Vec<_>, _>>()?;

    // Create a sidecar from the blob bytes (blocking)
    BlobTransactionSidecar::try_from_blobs_bytes(blobs)
        .map_err(|e| BlobError::KZGError(Box::new(e)))
}

/// Encodes the provided input data into a list of blobs, and returns a sidecar.
///
/// This function is async and uses a blocking thread pool with custom stack size for blob encoding.
pub async fn create_blob_sidecar_from_data_async(
    data: Bytes,
) -> Result<BlobTransactionSidecar, BlobError> {
    BLOB_THREAD_POOL
        .spawn_blocking(move || create_blob_sidecar_from_data_blocking(&data))
        .await
        .map_err(BlobError::ThreadPanicked)?
}

/// Decodes the provided blobs into a contiguous byte buffer.
///
/// NOTE: Blobs that are not in the provided hash set are skipped.
pub async fn decode_blobs_into_bytes_async(
    blobs: Vec<BlobSidecar>,
    blob_hashes: Vec<B256>,
) -> Result<Bytes, BlobError> {
    BLOB_THREAD_POOL
        .spawn_blocking(move || {
            let mut decoded_blobs_data = Vec::new();
            for blob in blobs {
                let versioned_hash = calculate_blob_versioned_hash(&blob.kzg_commitment);
                if blob_hashes.contains(&versioned_hash) {
                    match decode_blob_data(&blob.blob) {
                        Ok(decoded) => decoded_blobs_data.extend_from_slice(&decoded),
                        Err(e) => return Err(e),
                    }
                }
            }

            Ok(Bytes::from(decoded_blobs_data))
        })
        .await
        .map_err(BlobError::ThreadPanicked)?
}

/// Encodes the provided input data into the blob.
///
/// The encoding scheme works in rounds. In each round we process 4 field elements (each 32 bytes).
/// Each field element is written in two parts: a 6‑bit value (at the beginning) and a 31‑byte
/// chunk. In round 0 the first field element reserves bytes [1..5] to encode the version and data
/// length.
///
/// Data bounds: 0 <= `data.len()` <= [`MAX_BLOB_DATA_SIZE`]
///
/// Ported from: <https://github.com/ethereum-optimism/optimism/blob/0e4b867e08ed4dfcb5f1a76693f17392b189a7f6/op-service/eth/blob.go#L90>
pub fn create_blob_from_data(data: &[u8]) -> Result<Bytes, BlobError> {
    let mut out = Blob::default();

    if data.len() > MAX_BLOB_DATA_SIZE {
        return Err(BlobError::InputTooLarge(data.len()));
    }

    let mut read_offset: usize = 0;
    let mut write_offset: usize = 0;

    // Process the input data in rounds.
    for round in 0..ROUNDS {
        if read_offset >= data.len() {
            break;
        }

        let x = if round == 0 {
            // For round 0, reserve the first 4 bytes of the first field element.
            let mut buf = [0u8; 31];
            buf[0] = ENCODING_VERSION;
            let ilen = data.len() as u32;
            buf[1] = ((ilen >> 16) & 0xFF) as u8;
            buf[2] = ((ilen >> 8) & 0xFF) as u8;
            buf[3] = (ilen & 0xFF) as u8;

            // Copy as many bytes as possible into buf starting at index 4.
            let available = 31 - 4;
            let n = std::cmp::min(available, data.len() - read_offset);
            buf[4..4 + n].copy_from_slice(&data[read_offset..read_offset + n]);
            read_offset += n;

            // First field element: encode one 6‑bit value from input.
            let x = read_one_byte(data, &mut read_offset);
            let six_bits_of_x = x & 0b0011_1111;
            write_one_byte(&mut out.0, &mut write_offset, six_bits_of_x);
            write_31_bytes(&mut out.0, &mut write_offset, &buf);

            x
        } else {
            // For subsequent rounds, fill buf from data.
            let buf = read_31_bytes(data, &mut read_offset);
            let x = read_one_byte(data, &mut read_offset);
            let six_bits_of_x = x & 0b0011_1111;
            write_one_byte(&mut out.0, &mut write_offset, six_bits_of_x);
            write_31_bytes(&mut out.0, &mut write_offset, &buf);

            x
        };

        // Second field element: combine bits from x and a new byte.
        let buf = read_31_bytes(data, &mut read_offset);
        let y = read_one_byte(data, &mut read_offset);
        let b = (y & 0b0000_1111) | ((x & 0b1100_0000) >> 2);
        write_one_byte(&mut out.0, &mut write_offset, b);
        write_31_bytes(&mut out.0, &mut write_offset, &buf);

        // Third field element: encode another 6‑bit value.
        let buf = read_31_bytes(data, &mut read_offset);
        let z = read_one_byte(data, &mut read_offset);
        let six_bits_of_z = z & 0b0011_1111;
        write_one_byte(&mut out.0, &mut write_offset, six_bits_of_z);
        write_31_bytes(&mut out.0, &mut write_offset, &buf);

        // Fourth field element: combine bits from y and z.
        let buf = read_31_bytes(data, &mut read_offset);
        let d = ((z & 0b1100_0000) >> 2) | ((y & 0b1111_0000) >> 4);
        write_one_byte(&mut out.0, &mut write_offset, d);
        write_31_bytes(&mut out.0, &mut write_offset, &buf);
    }

    if read_offset < data.len() {
        return Err(BlobError::DataDidNotFit { read_offset, data_len: data.len() });
    }

    Ok(Bytes::from(out.0))
}

/// Decode a blob into its original data.
///
/// Adapted from: <https://github.com/a16z/magi/blob/master/src/l1/blob_encoding.rs>
pub fn decode_blob_data(blob: &[u8]) -> Result<Bytes, BlobError> {
    let mut output = vec![0; MAX_BLOB_DATA_SIZE];

    if blob[VERSION_OFFSET] != ENCODING_VERSION {
        return Err(BlobError::InvalidVersion { want: ENCODING_VERSION, got: blob[VERSION_OFFSET] });
    }

    // decode the 3-byte big-endian length value into a 4-byte integer
    let output_len = u32::from_be_bytes([0, blob[2], blob[3], blob[4]]) as usize;
    if output_len > MAX_BLOB_DATA_SIZE {
        return Err(BlobError::InvalidLength { got: output_len, maximum: MAX_BLOB_DATA_SIZE });
    }

    output[0..27].copy_from_slice(&blob[5..32]);

    let mut output_pos = 28;
    let mut input_pos = 32;

    // buffer for the 4 6-bit chunks
    let mut encoded_byte = [0; 4];

    encoded_byte[0] = blob[0];
    for byte in encoded_byte.iter_mut().skip(1) {
        *byte = decode_field_element(&mut output_pos, &mut input_pos, blob, &mut output)?;
    }
    reassemble_bytes(&mut output_pos, encoded_byte, &mut output);

    for _ in 1..ROUNDS {
        if output_pos >= output_len {
            break;
        }

        for byte in &mut encoded_byte {
            *byte = decode_field_element(&mut output_pos, &mut input_pos, blob, &mut output)?;
        }
        reassemble_bytes(&mut output_pos, encoded_byte, &mut output);
    }

    for output_byte in output.iter().take(MAX_BLOB_DATA_SIZE).skip(output_len) {
        if output_byte != &0 {
            return Err(BlobError::ExtraneousData(format!("field element {}", output_pos / 32)));
        }
    }

    output.truncate(output_len);

    for byte in blob.iter().skip(input_pos) {
        if byte != &0 {
            return Err(BlobError::ExtraneousData(format!("input position {}", input_pos)));
        }
    }

    Ok(output.into())
}

/// Helper function for decoding field elements.
fn decode_field_element(
    output_pos: &mut usize,
    input_pos: &mut usize,
    blob: &[u8],
    output: &mut [u8],
) -> Result<u8, BlobError> {
    let result = blob[*input_pos];

    // two highest order bits of the first byte of each field element should always be 0
    if result & 0b1100_0000 != 0 {
        return Err(BlobError::InvalidFieldElement);
    }

    output[*output_pos..*output_pos + 31].copy_from_slice(&blob[*input_pos + 1..*input_pos + 32]);

    *output_pos += 32;
    *input_pos += 32;

    Ok(result)
}

/// Helper function for reassembling bytes.
fn reassemble_bytes(output_pos: &mut usize, encoded_byte: [u8; 4], output: &mut [u8]) {
    *output_pos -= 1;

    let x = (encoded_byte[0] & 0b0011_1111) | ((encoded_byte[1] & 0b0011_0000) << 2);
    let y = (encoded_byte[1] & 0b0000_1111) | ((encoded_byte[3] & 0b0000_1111) << 4);
    let z = (encoded_byte[2] & 0b0011_1111) | ((encoded_byte[3] & 0b0011_0000) << 2);

    output[*output_pos - 32] = z;
    output[*output_pos - (32 * 2)] = y;
    output[*output_pos - (32 * 3)] = x;
}

/// Helper functions for reading from a single byte from the input data,
/// while advancing the read offset.
fn read_one_byte(data: &[u8], read_offset: &mut usize) -> u8 {
    if *read_offset < data.len() {
        let b = data[*read_offset];
        *read_offset += 1;
        b
    } else {
        0
    }
}

/// Helper functions for reading 31 bytes from the input data,
/// while advancing the read offset.
fn read_31_bytes(data: &[u8], read_offset: &mut usize) -> [u8; 31] {
    let mut buf = [0u8; 31];
    if *read_offset < data.len() {
        let n = std::cmp::min(31, data.len() - *read_offset);
        buf[..n].copy_from_slice(&data[*read_offset..*read_offset + n]);
        *read_offset += n;
    }
    buf
}

/// Helper functions for writing one byte to the output blob, while
/// advancing the write offset.
fn write_one_byte(out: &mut [u8; BYTES_PER_BLOB], write_offset: &mut usize, v: u8) {
    assert!(
        (*write_offset % 32 == 0),
        "blob encoding: invalid byte write offset: {}",
        *write_offset
    );

    assert!((v & 0b1100_0000 == 0), "blob encoding: invalid 6-bit value: {:08b}", v);

    out[*write_offset] = v;
    *write_offset += 1;
}

/// Helper function for writing a 31-byte chunk to the output blob, while
/// advancing the write offset.
fn write_31_bytes(out: &mut [u8; BYTES_PER_BLOB], write_offset: &mut usize, buf: &[u8; 31]) {
    assert!(
        (*write_offset % 32 == 1),
        "blob encoding: invalid bytes31 write offset: {}",
        *write_offset
    );

    out[*write_offset..*write_offset + 31].copy_from_slice(buf);
    *write_offset += 31;
}

/// Helper function for calculating the versioned hash of a KZG commitment.
///
/// Reference: <https://github.com/ethereum/EIPs/blob/master/EIPS/eip-4844.md#helpers>
fn calculate_blob_versioned_hash(kzg_commitment: &KzgCommitment) -> B256 {
    let mut versioned_hash = B256::ZERO;
    versioned_hash[0] = VERSIONED_HASH_VERSION_KZG;
    versioned_hash
        .get_mut(1..)
        .expect("non empty hash")
        .copy_from_slice(&keccak256(kzg_commitment)[1..]);
    versioned_hash
}

#[cfg(test)]
mod tests {

    use alloy::{
        consensus::{Blob, BlobTransactionSidecar, SidecarBuilder, SimpleCoder},
        hex,
    };

    use crate::blob::{
        BlobError, BlobTxFeesHints, MAX_BLOB_DATA_SIZE, VERSION_OFFSET, create_blob_from_data,
        decode_blob_data,
    };
    use alloy::eips::{eip1559::Eip1559Estimation, eip4844::BYTES_PER_BLOB};

    #[test]
    fn test_into_blob_sidecar_single() {
        // Read the test file as a string (it's hex-encoded)
        // https://etherscan.io/tx/0xa92219c86575de1b3dd8752159bc41b9e4c6ed2065b5543fac1d826f7b979018
        let hex_data = std::fs::read_to_string(
            "test_data/blobs/0xa92219c86575de1b3dd8752159bc41b9e4c6ed2065b5543fac1d826f7b979018",
        )
        .expect("file not found");

        // Remove the "0x" prefix if present and decode from hex.
        let data = if let Some(stripped) = hex_data.strip_prefix("0x") {
            hex::decode(stripped).expect("invalid hex")
        } else {
            hex_data.as_bytes().to_vec()
        };

        // Convert the input into a BlobTransactionSidecar.
        let sidecar = BlobTransactionSidecar::try_from_blobs_bytes(vec![Blob::from_slice(&data)])
            .expect("to work");
        assert_eq!(sidecar.blobs.len(), 1, "There should be 1 blob");

        // Commitments and proofs should match the number of blobs.
        assert_eq!(sidecar.commitments.len(), 1, "There should be 1 commitment");
        assert_eq!(sidecar.proofs.len(), 1, "There should be 1 proof");
    }

    #[test]
    fn test_try_from_blobs_bytes() {
        let blob = vec![Blob::default()];
        let sidecar = BlobTransactionSidecar::try_from_blobs_bytes(blob).expect("to work");
        assert_eq!(sidecar.blobs.len(), 1);
    }

    #[test]
    fn test_try_from_generated_blob() {
        let blob_bytes = std::fs::read("test_data/blobs/generated_blob").expect("file not found");
        let blob_bytes = Blob::from_slice(&blob_bytes);

        let sidecar =
            SidecarBuilder::<SimpleCoder>::from_slice(blob_bytes.0.as_ref()).build().unwrap();
        assert_eq!(sidecar.blobs.len(), 2);
    }

    #[test]
    fn bump_base_fee_works() {
        let mut hints = BlobTxFeesHints::new(10_000, 10_000_000, 5);
        // max priority fee is less than 1/10 of the base fee
        hints.bump_base_fee(12_000_000);
        assert_eq!(hints.max_priority_fee_per_gas(), 1_200_000);
        assert_eq!(hints.max_fee_per_gas(), 12_000_000 + 1_200_000);
        assert_eq!(hints.blob_fee_per_gas(), 5);

        let mut hints = BlobTxFeesHints::new(5_000_000, 10_000_000, 5);
        // max priority fee is greater than 1/10 of the base fee
        hints.bump_base_fee(12_000_000);
        assert_eq!(hints.max_priority_fee_per_gas(), 5_000_000);
        assert_eq!(hints.max_fee_per_gas(), 12_000_000 + 5_000_000);
        assert_eq!(hints.blob_fee_per_gas(), 5);

        let mut hints = BlobTxFeesHints::new(5_000_000, 10_000_000, 5);
        // No-op if the base fee is not increased
        hints.bump_base_fee(0);
        assert_eq!(hints.max_priority_fee_per_gas(), 5_000_000);
        assert_eq!(hints.max_fee_per_gas(), 10_000_000);
        assert_eq!(hints.blob_fee_per_gas(), 5);
    }

    #[test]
    fn next_block_increase_works() {
        let hints = BlobTxFeesHints::new(1_000, 10_000, 5);

        let increased = hints.with_next_block_increase();

        assert_eq!(increased.max_priority_fee_per_gas(), 1_000);
        assert_eq!(increased.max_fee_per_gas(), 11_126);
        assert_eq!(increased.blob_fee_per_gas(), 6);
    }

    #[test]
    fn enforce_min_batch_tip_wei_works() {
        let hints = BlobTxFeesHints::new(100, 300, 10);

        // tip lower than current priority fee -> unchanged
        let unchanged = hints.with_enforced_min_batch_tip_wei(50);
        assert_eq!(unchanged.max_priority_fee_per_gas(), 100);
        assert_eq!(unchanged.max_fee_per_gas(), 300);
        assert_eq!(unchanged.blob_fee_per_gas(), 10);

        // tip higher than current priority fee -> bumped
        let bumped = hints.with_enforced_min_batch_tip_wei(150);
        assert_eq!(bumped.max_priority_fee_per_gas(), 151);
        assert_eq!(bumped.max_fee_per_gas(), 351);
        assert_eq!(bumped.blob_fee_per_gas(), 11);

        // tip equal to current priority fee -> no-op
        let bumped_equal = hints.with_enforced_min_batch_tip_wei(100);
        assert_eq!(bumped_equal.max_priority_fee_per_gas(), 100);
        assert_eq!(bumped_equal.max_fee_per_gas(), 300);
        assert_eq!(bumped_equal.blob_fee_per_gas(), 10);
    }

    #[test]
    fn from_estimation_uses_max_fee() {
        let estimation = Eip1559Estimation { max_fee_per_gas: 1000, max_priority_fee_per_gas: 5 };

        let hints = BlobTxFeesHints::from_estimation(estimation, 7);

        assert_eq!(hints.max_priority_fee_per_gas(), 5);
        assert_eq!(hints.max_fee_per_gas(), 1000);
        assert_eq!(hints.blob_fee_per_gas(), 7);
    }

    #[test]
    fn blob_encode_decode() {
        let cases: [&[u8]; 8] = [
            b"this is a test of blob encoding/decoding",
            b"short",
            b"\x00",
            b"\x00\x01\x00",
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            b"",
        ];

        for data in &cases {
            let blob = create_blob_from_data(data).expect("encode");
            let decoded = decode_blob_data(&blob).expect("decode");
            assert_eq!(*data, decoded.as_ref());
        }
    }

    #[test]
    fn small_blob_encoding() {
        // the first field element is filled and no data remains
        let mut data = vec![0u8; 128];
        data[127] = 0xFF;
        let blob = create_blob_from_data(&data).expect("encode");
        let decoded = decode_blob_data(&blob).expect("decode");
        assert_eq!(data, decoded.as_ref());

        // only 10 bytes of data
        let mut data = vec![0u8; 10];
        data[9] = 0xFF;
        let blob = create_blob_from_data(&data).expect("encode");
        let decoded = decode_blob_data(&blob).expect("decode");
        assert_eq!(data, decoded.as_ref());

        // no 3 bytes of extra data left to encode after the first 4 field elements
        let mut data = vec![0u8; 27 + 31 * 3];
        data[27 + 31 * 3 - 1] = 0xFF;
        let blob = create_blob_from_data(&data).expect("encode");
        let decoded = decode_blob_data(&blob).expect("decode");
        assert_eq!(data, decoded.as_ref());
    }

    #[test]
    fn big_blob_encoding() {
        use rand::{RngCore, SeedableRng, rngs::StdRng};

        let mut rng = StdRng::seed_from_u64(99);
        let mut big_data = vec![0u8; MAX_BLOB_DATA_SIZE];
        rng.fill_bytes(&mut big_data);

        // test the maximum size of data that can be encoded
        let blob = create_blob_from_data(&big_data).expect("encode");
        let decoded = decode_blob_data(&blob).expect("decode");
        assert_eq!(big_data, decoded.as_ref());

        // perform encode/decode test on progressively smaller inputs to exercise boundary
        // conditions
        for i in 1..256 {
            let temp = &big_data[i..];
            let blob = create_blob_from_data(temp).expect("encode");
            let decoded = decode_blob_data(&blob).expect("decode");
            assert_eq!(temp.len(), decoded.len());
            assert_eq!(temp, decoded.as_ref());
        }
    }

    #[test]
    fn invalid_blob_decoding() {
        let data = b"this is a test of invalid blob decoding";
        let original = create_blob_from_data(data).expect("encode").to_vec();

        let mut blob = original.clone();
        blob[32] = 0b1000_0000;
        let err = decode_blob_data(&blob).expect_err("should fail");
        assert!(matches!(err, BlobError::InvalidFieldElement));

        let mut blob = original.clone();
        blob[32] = 0b0100_0000;
        let err = decode_blob_data(&blob).expect_err("should fail");
        assert!(matches!(err, BlobError::InvalidFieldElement));

        let mut blob = original.clone();
        blob[VERSION_OFFSET] = 0x01;
        let err = decode_blob_data(&blob).expect_err("should fail");
        assert!(matches!(err, BlobError::InvalidVersion { .. }));

        let mut blob = original;
        blob[2] = 0xFF;
        let err = decode_blob_data(&blob).expect_err("should fail");
        assert!(matches!(err, BlobError::InvalidLength { .. }));
    }

    #[test]
    fn too_long_data_encoding() {
        let data = vec![0u8; BYTES_PER_BLOB];
        let err = create_blob_from_data(&data).expect_err("should fail");
        assert!(matches!(err, BlobError::InputTooLarge(_)));
    }

    #[test]
    fn decode_test_vectors() {
        enum ErrKind {
            InvalidLength,
        }
        struct Case {
            input: &'static [u8],
            output: Vec<u8>,
            err: Option<ErrKind>,
        }

        let cases = [
            Case { input: b"", output: Vec::new(), err: None },
            Case { input: b"\x00\x00\x00\x00\x01", output: vec![0], err: None },
            Case {
                input: b"\x00\x00\x01\xFB\xFC",
                output: vec![0u8; 130_044],
                err: None,
            },
            Case {
                input: b"\x00\x00\x01\xFB\xFD",
                output: Vec::new(),
                err: Some(ErrKind::InvalidLength),
            },
            Case {
                input: b"\x00\x00\x00\x00\x0a\x00\x00\x00\x00\x00\x00\x00\x00\x00\xff",
                output: vec![
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff,
                ],
                err: None,
            },
            Case {
                input: b"\x00\x00\x00\x00\x1b\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff",
                output: vec![0xffu8; 27],
                err: None,
            },
            Case {
                input: b"\x3f\x00\x00\x00\x20\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x30\xff\xff\xff\xff",
                output: vec![0xffu8; 32],
                err: None,
            },
        ];

        for case in &cases {
            let mut blob = vec![0u8; BYTES_PER_BLOB];
            blob[..case.input.len()].copy_from_slice(case.input);
            match decode_blob_data(&blob) {
                Ok(decoded) => {
                    assert!(case.err.is_none(), "expected error");
                    assert_eq!(case.output.len(), decoded.len());
                    assert_eq!(case.output, decoded.as_ref());
                }
                Err(err) => match case.err {
                    Some(ErrKind::InvalidLength) => {
                        assert!(matches!(err, BlobError::InvalidLength { .. }));
                    }
                    None => panic!("unexpected error: {:?}", err),
                },
            }
        }
    }

    #[test]
    fn encode_test_vectors() {
        enum ErrKind {
            InputTooLarge,
        }
        struct Case {
            input: &'static [u8],
            output: Vec<u8>,
            err: Option<ErrKind>,
        }

        let cases = [
            Case { input: b"", output: Vec::new(), err: None },
            Case {
                input: &[0u8; MAX_BLOB_DATA_SIZE],
                output: vec![0x00, 0x00, 0x01, 0xfb, 0xfc],
                err: None,
            },
            Case {
                input: &[0u8; MAX_BLOB_DATA_SIZE + 1],
                output: Vec::new(),
                err: Some(ErrKind::InputTooLarge),
            },
            Case {
                input: &[0xffu8; 27],
                output: {
                    let mut v = vec![0u8; 32];
                    v[4] = 0x1b;
                    for b in &mut v[5..] {
                        *b = 0xff;
                    }
                    v
                },
                err: None,
            },
            Case {
                input: &[0xffu8; 28],
                output: {
                    let mut v = vec![0u8; 33];
                    v[0] = 0x3f;
                    v[4] = 0x1c;
                    for b in &mut v[5..32] {
                        *b = 0xff;
                    }
                    v[32] = 0x30;
                    v
                },
                err: None,
            },
        ];

        for (idx, case) in cases.iter().enumerate() {
            match create_blob_from_data(case.input) {
                Ok(blob) => {
                    assert!(case.err.is_none(), "expected error");
                    let mut expected = vec![0u8; BYTES_PER_BLOB];
                    expected[..case.output.len()].copy_from_slice(&case.output);
                    assert_eq!(expected, blob.to_vec(), "case {}", idx);
                }
                Err(err) => match case.err {
                    Some(ErrKind::InputTooLarge) => {
                        assert!(matches!(err, BlobError::InputTooLarge(_)));
                    }
                    None => panic!("unexpected error: {:?}", err),
                },
            }
        }
    }

    #[test]
    fn extraneous_data() {
        let mut blob = vec![0u8; BYTES_PER_BLOB];

        blob[..5].copy_from_slice(b"\x30\x00\x00\x00\x00");
        let err = decode_blob_data(&blob).expect_err("should fail");
        assert!(matches!(err, BlobError::ExtraneousData(_)));

        blob[..5].copy_from_slice(b"\x01\x00\x00\x00\x00");
        let err = decode_blob_data(&blob).expect_err("should fail");
        assert!(matches!(err, BlobError::ExtraneousData(_)));

        blob.fill(0);
        let mut i = 5usize;
        while i < 128 {
            blob[i - 1] = 0;
            blob[i] = 1;
            let err = decode_blob_data(&blob).expect_err("should fail");
            assert!(matches!(err, BlobError::ExtraneousData(_)));
            i += 1;
        }

        while i < BYTES_PER_BLOB {
            blob[i - 1] = 0;
            blob[i] = 1;
            let err = decode_blob_data(&blob).expect_err("should fail");
            assert!(matches!(err, BlobError::ExtraneousData(_)));
            i += 7;
        }
    }
}
