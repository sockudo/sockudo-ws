#![cfg(feature = "permessage-deflate")]

use sockudo_ws::compression::{CompressionContext, SharedCompressorPool};
use sockudo_ws::deflate::{DeflateConfig, DeflateEncoder};
use std::sync::Arc;

#[test]
fn shared_client_encoder_uses_the_client_window() {
    let config = DeflateConfig {
        server_max_window_bits: sockudo_ws::DeflateWindowBits::Bits15,
        client_max_window_bits: sockudo_ws::DeflateWindowBits::Bits10,
        compression_threshold: 0,
        ..DeflateConfig::default()
    };
    let mut state = 0x1234_5678_u32;
    let prefix: Vec<u8> = (0..2048)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let payload = [prefix.as_slice(), prefix.as_slice(), &[b'a'; 8192]].concat();
    let mut reference_encoder = DeflateEncoder::new(
        sockudo_ws::DeflateWindowBits::Bits10,
        true,
        config.compression_level,
        0,
    );
    let expected_compressed = reference_encoder.compress(&payload).unwrap();
    let pool = Arc::new(SharedCompressorPool::new(config));
    let mut client = CompressionContext::with_shared_pool(pool, false);

    // Avoid dumping multi-kilobyte compressed buffers if this assertion fails.
    assert!(client.compress(&payload).unwrap() == expected_compressed);
}
