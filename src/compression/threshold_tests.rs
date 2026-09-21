use std::sync::atomic::Ordering;

use super::*;
use crate::deflate::DeflateWindowBits;

#[test]
fn shared_pool_skips_selection_below_threshold() {
    let pool = SharedCompressorPool::new(DeflateConfig {
        compression_threshold: 64,
        ..DeflateConfig::default()
    });

    assert!(pool.compress(&[b'a'; 63]).unwrap().is_none());

    assert_eq!(pool.inner.server.next_encoder.load(Ordering::Relaxed), 0);
}

#[test]
fn shared_threshold_preserves_message_round_trips() {
    for through_context in [false, true] {
        let pool = Arc::new(SharedCompressorPool::new(DeflateConfig {
            compression_threshold: 64,
            ..DeflateConfig::default()
        }));
        let mut context = CompressionContext::with_shared_pool(pool.clone(), true);
        let mut decoder = DeflateDecoder::new(DeflateWindowBits::Bits15, true);

        // Revisit every pool slot after skipped messages to detect changed history.
        for _ in 0..3 {
            for size in [64, 63, 65] {
                let data = vec![b'a'; size];
                let compressed = if through_context {
                    context.compress(&data)
                } else {
                    pool.compress(&data)
                }
                .unwrap();
                assert_eq!(compressed.is_some(), size >= 64);
                let decoded = match compressed {
                    Some(bytes) => decoder.decompress(&bytes, 1024).unwrap(),
                    None => Bytes::copy_from_slice(&data),
                };
                assert_eq!(decoded.as_ref(), data);
            }
        }
        assert_eq!(pool.inner.server.next_encoder.load(Ordering::Relaxed), 6);
    }
}

#[test]
fn split_shared_encoder_skips_selection_below_threshold() {
    for is_server in [false, true] {
        let pool = Arc::new(SharedCompressorPool::new(DeflateConfig {
            compression_threshold: 64,
            client_max_window_bits: DeflateWindowBits::Bits10,
            ..DeflateConfig::default()
        }));
        let context = CompressionContext::with_shared_pool(pool.clone(), is_server);
        let (mut encoder, _) = context.into_parts();
        let bits = if is_server {
            pool.config().server_max_window_bits
        } else {
            pool.config().client_max_window_bits
        };
        let mut decoder = DeflateDecoder::new(bits, true);

        for size in [64, 63, 65] {
            let data = vec![b'b'; size];
            let compressed = encoder.compress(&data).unwrap();
            assert_eq!(compressed.is_some(), size >= 64);
            if let Some(bytes) = compressed {
                assert_eq!(decoder.decompress(&bytes, 1024).unwrap().as_ref(), data);
            }
        }
        assert_eq!(
            pool.inner.server.next_encoder.load(Ordering::Relaxed),
            if is_server { 2 } else { 0 }
        );
        assert_eq!(
            pool.inner.client.next_encoder.load(Ordering::Relaxed),
            if is_server { 0 } else { 2 }
        );
    }
}
