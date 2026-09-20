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
fn decompression_drains_output_after_filling_the_initial_capacity() {
    use flate2::{Compress, Compression, FlushCompress};

    // A random prefix keeps the compressed payload large. Find adjacent
    // payloads where one exactly fills the decoder's initial 4x capacity and
    // the other has one more output byte pending with the same input length.
    let mut random = 0x9e37_79b9_7f4a_7c15u64;
    let prefix: Vec<u8> = (0..8192)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random as u8
        })
        .collect();
    let compress = |run: usize| {
        let payload = [prefix.clone(), vec![b'A'; run]].concat();
        let mut encoder = Compress::new_with_window_bits(Compression::new(6), false, 15);
        let mut output = Vec::with_capacity(payload.len() + 64);
        encoder
            .compress_vec(&payload, &mut output, FlushCompress::Sync)
            .unwrap();
        assert!(output.ends_with(&[0, 0, 0xff, 0xff]));
        output.truncate(output.len() - 4);
        (payload, output)
    };
    let (exact, longer) = (prefix.len() * 3..prefix.len() * 4)
        .map(|run| (compress(run), compress(run + 1)))
        .find(|((payload, compressed), (_, longer))| {
            payload.len() == compressed.len() * 4 && longer.len() == compressed.len()
        })
        .expect("a payload exactly four times its compressed length");
    let limit = exact.0.len();

    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);
    assert_eq!(
        decoder.decompress(&exact.1, limit).unwrap().as_ref(),
        exact.0
    );
    assert_eq!(
        decoder.decompress(&exact.1, limit).unwrap().as_ref(),
        exact.0
    );

    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);
    assert!(matches!(
        decoder.decompress(&longer.1, limit),
        Err(Error::MessageTooLarge)
    ));
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);
    assert_eq!(
        decoder.decompress(&longer.1, limit + 1).unwrap().as_ref(),
        longer.0
    );
}
