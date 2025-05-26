use std::{
    self,
    io::{Read, Write},
};

use alloy_primitives::Bytes;
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use tokio::sync::oneshot;
use tracing::error;

mod estimate;
pub use estimate::ZlibCompressionEstimate;

/// Compress the input bytes using `zlib`.
pub fn zlib_compress(input: &[u8]) -> std::io::Result<Bytes> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input)?;
    encoder.finish().map(Bytes::from)
}

/// Decompress the input bytes using `zlib`.
pub fn zlib_decompress(input: &[u8]) -> std::io::Result<Bytes> {
    let mut decoder = ZlibDecoder::new(input);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(Bytes::from(decompressed))
}

/// RLP-encode and compress with zlib a given encodable object.
pub fn rlp_encode_and_compress<E: alloy_rlp::Encodable>(b: &E) -> std::io::Result<Bytes> {
    let rlp_encoded_tx_list = alloy_rlp::encode(b);
    zlib_compress(&rlp_encoded_tx_list)
}

/// RLP-encode and compress with zlib the given tx lists in a background thread.
/// Returns a oneshot channel that will receive the compressed size of the tx lists.
pub fn rlp_encode_and_compress_in_background<E>(b: E) -> oneshot::Receiver<usize>
where
    E: alloy_rlp::Encodable + Send + Sync + 'static,
{
    let (tx, rx) = oneshot::channel();

    tokio::task::spawn_blocking(move || {
        let rlp_encoded_tx_list = alloy_rlp::encode(b);
        let _ = match zlib_compress(&rlp_encoded_tx_list) {
            Ok(compressed) => tx.send(compressed.len()),
            Err(e) => {
                error!(?e, "Error while compressing tx lists in the background");
                tx.send(0)
            }
        };
    });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_and_compress_tx_list() {
        // Generate 200 transactions with different values
        let txs: Vec<Bytes> =
            (0..200).map(|i| Bytes::from(format!("0x{:04x}", i).into_bytes())).collect();

        let uncompressed_size: usize = txs.iter().map(|tx| tx.len()).sum();
        assert_eq!(uncompressed_size, 1200, "Each tx should be 6 bytes (200 * 6 = 1200)");

        let compressed = rlp_encode_and_compress(&txs).unwrap();
        assert_eq!(compressed.len(), 333, "Compressed size should be 333 bytes");

        // Compressed size should be less than uncompressed size
        assert!(
            compressed.len() < uncompressed_size,
            "Compressed size ({}) should be less than uncompressed size ({})",
            compressed.len(),
            uncompressed_size
        );
    }
}
