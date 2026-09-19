#![cfg(feature = "permessage-deflate")]
use sockudo_ws::Error;
use sockudo_ws::deflate::{DeflateDecoder, DeflateEncoder};

#[test]
fn decompression_handles_long_payloads_that_exactly_fill_the_output_capacity() {
    use flate2::{Compress, Compression, FlushCompress};

    // A random prefix keeps the compressed payload longer than the inline
    // input buffer; the run length is searched so the decompressed length is
    // exactly four times the compressed length, the initial output capacity.
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
    // The stored prefix costs five bytes and the run a few dozen, so the
    // match lies a little above three prefix lengths.
    let (exact, longer) = (prefix.len() * 3..prefix.len() * 4)
        .map(|run| (compress(run), compress(run + 1)))
        .find(|((payload, compressed), (_, longer))| {
            payload.len() == compressed.len() * 4 && longer.len() == compressed.len()
        })
        .expect("a payload exactly four times its compressed length");
    assert!(exact.1.len() > 4096);
    let limit = exact.0.len();

    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);
    assert_eq!(
        decoder.decompress(&exact.1, limit).unwrap().as_ref(),
        exact.0
    );
    // The decoder must be at a block boundary for the next message.
    assert_eq!(
        decoder.decompress(&exact.1, limit).unwrap().as_ref(),
        exact.0
    );

    // One more byte than the limit is pending once the output is full.
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

#[test]
fn decompression_skips_the_trailer_after_a_long_final_block_that_fills_the_output() {
    use flate2::{Compress, Compression, FlushCompress};

    // A random prefix keeps the payload on the path that inflates the trailer
    // separately. The run gets its own final block of a few short codes, and
    // every run length whose output crosses the initial capacity inside that
    // block is checked, so some leave a match pending after all bits are read.
    let mut random = 0x2545_f491_4f6c_dd1du64;
    let prefix: Vec<u8> = (0..8192)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random as u8
        })
        .collect();
    let compress = |run: usize| {
        let mut encoder = Compress::new_with_window_bits(Compression::new(6), false, 15);
        let mut output = Vec::with_capacity(prefix.len() + 256);
        encoder
            .compress_vec(&prefix, &mut output, FlushCompress::Sync)
            .unwrap();
        encoder
            .compress_vec(&vec![b'A'; run], &mut output, FlushCompress::Finish)
            .unwrap();
        output
    };
    let next = vec![b'B'; 32];
    let next_compressed = DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true, 6, 0)
        .compress(&next)
        .unwrap()
        .unwrap();
    let first_run = compress(0).len() * 4 - prefix.len();
    let mut crossings = 0;
    for run in first_run..first_run + 2048 {
        let compressed = compress(run);
        let payload_len = prefix.len() + run;
        if !(1..258).contains(&payload_len.saturating_sub(compressed.len() * 4)) {
            continue;
        }
        crossings += 1;
        let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

        let decoded = decoder.decompress(&compressed, payload_len).unwrap();
        // A trailer fed to the fresh stream would leave a partial stored-block header.
        let decoded_next = decoder.decompress(&next_compressed, next.len());

        assert_eq!(
            decoded.as_ref(),
            [prefix.as_slice(), &vec![b'A'; run]].concat()
        );
        assert_eq!(decoded_next.unwrap().as_ref(), next);
    }
    assert!(crossings > 0);
}
