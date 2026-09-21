#![cfg(feature = "permessage-deflate")]

use sockudo_ws::Error;
use sockudo_ws::deflate::{DeflateDecoder, DeflateEncoder};

#[test]
fn decompression_accepts_a_valid_final_block() {
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    let decoded = decoder
        .decompress(&[0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00], 5)
        .unwrap();

    assert_eq!(decoded.as_ref(), b"Hello");
}

#[test]
fn decompression_rejects_output_over_limit_in_the_final_call() {
    let payload = vec![b'A'; 128];
    let compressed = DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true, 6, 0)
        .compress(&payload)
        .unwrap()
        .unwrap();
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);

    let result = decoder.decompress(&compressed, payload.len() - 1);

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[test]
fn decompression_accepts_exact_limits_across_output_growth() {
    for size in [0, 32, 1024, 1025, 5120, 65536] {
        let payload = vec![b'A'; size];
        let compressed = if size == 0 {
            // An empty sync-flushed DEFLATE block with the four-byte trailer removed.
            vec![0].into()
        } else {
            DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true, 6, 0)
                .compress(&payload)
                .unwrap()
                .unwrap()
        };
        let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);

        let result = decoder.decompress(&compressed, size).unwrap();

        assert_eq!(result.as_ref(), payload);
    }
}

#[test]
fn exact_limit_probe_preserves_context_takeover() {
    let payload = vec![b'A'; 1024];
    let mut encoder = DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false, 6, 0);
    let first = encoder.compress(&payload).unwrap().unwrap();
    let second = encoder.compress(&payload).unwrap().unwrap();
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    assert_eq!(decoder.decompress(&first, 1024).unwrap().as_ref(), payload);
    assert_eq!(decoder.decompress(&second, 1024).unwrap().as_ref(), payload);
}
