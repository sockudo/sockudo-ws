#![cfg(feature = "permessage-deflate")]

use sockudo_ws::Error;
use sockudo_ws::deflate::{DeflateContext, DeflateDecoder, DeflateEncoder};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn public_deflate_types_remain_send_and_sync() {
    assert_send_sync::<DeflateDecoder>();
    assert_send_sync::<DeflateContext>();

    #[cfg(feature = "tokio-runtime")]
    assert_send_sync::<sockudo_ws::CompressedWebSocketStream<tokio::net::TcpStream>>();
}

fn finish_deflate(payload: &[u8], dictionary: Option<&[u8]>) -> Vec<u8> {
    use flate2::{Compress, Compression, FlushCompress, Status};

    let mut encoder = Compress::new_with_window_bits(Compression::new(6), false, 15);
    if let Some(dictionary) = dictionary {
        encoder.set_dictionary(dictionary).unwrap();
    }
    let mut output = vec![0; payload.len() + 64];
    let status = encoder
        .compress(payload, &mut output, FlushCompress::Finish)
        .unwrap();
    assert_eq!(status, Status::StreamEnd);
    output.truncate(encoder.total_out() as usize);
    // RFC 7692 requires the header byte of the following empty stored block
    // after a final DEFLATE block so the payload uses the standard transform.
    output.push(0);
    output
}

#[test]
fn decompression_accepts_the_rfc_final_block_example() {
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    let decoded = decoder
        .decompress(&[0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00], 5)
        .unwrap();

    assert_eq!(decoded.as_ref(), b"Hello");
}

#[test]
fn decompression_accepts_final_blocks_with_context_takeover() {
    let first = vec![b'A'; 1024];
    let second = [vec![b'A'; 768], vec![b'B'; 256]].concat();
    let first_compressed = finish_deflate(&first, None);
    let second_compressed = finish_deflate(&second, Some(&first));
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    assert_eq!(
        decoder
            .decompress(&first_compressed, 1024)
            .unwrap()
            .as_ref(),
        first
    );
    assert_eq!(
        decoder
            .decompress(&second_compressed, 1024)
            .unwrap()
            .as_ref(),
        second
    );
}

#[test]
fn decompression_accepts_consecutive_final_blocks_without_context_takeover() {
    let payloads = [vec![b'A'; 1024], vec![b'B'; 1024]];
    let compressed = payloads
        .each_ref()
        .map(|payload| finish_deflate(payload, None));
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);

    for (input, expected) in compressed.iter().zip(&payloads) {
        assert_eq!(
            decoder.decompress(input, expected.len()).unwrap().as_ref(),
            expected
        );
    }
}

#[test]
fn decompression_rejects_invalid_data_after_a_final_block() {
    let payload = vec![b'A'; 1024];
    let mut compressed = finish_deflate(&payload, None);
    compressed.push(0x07);
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    let result = decoder.decompress(&compressed, payload.len());

    assert!(matches!(result, Err(Error::Compression(_))));
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
fn decompression_preserves_owned_messages_with_and_without_context_takeover() {
    for no_context_takeover in [false, true] {
        let mut encoder = DeflateEncoder::new(
            sockudo_ws::deflate::MAX_WINDOW_BITS,
            no_context_takeover,
            6,
            0,
        );
        let mut decoder =
            DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, no_context_takeover);
        let payloads: Vec<Vec<u8>> = [32, 4096, 128, 65536, 32]
            .into_iter()
            .map(|size| (0..size).map(|index| b'A' + (index & 3) as u8).collect())
            .collect();
        let compressed: Vec<_> = payloads
            .iter()
            .map(|payload| encoder.compress(payload).unwrap().unwrap())
            .collect();

        let decoded: Vec<_> = compressed
            .iter()
            .map(|input| decoder.decompress(input, 65536).unwrap())
            .collect();

        // Keep every result alive across later calls and input-buffer reuse.
        for (actual, expected) in decoded.iter().zip(&payloads) {
            assert_eq!(actual.as_ref(), expected);
        }
    }
}

#[test]
fn decompression_respects_a_smaller_limit_after_a_large_message() {
    for no_context_takeover in [false, true] {
        let mut encoder = DeflateEncoder::new(
            sockudo_ws::deflate::MAX_WINDOW_BITS,
            no_context_takeover,
            6,
            0,
        );
        let mut decoder =
            DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, no_context_takeover);
        let large = vec![b'A'; 65536];
        let input = encoder.compress(&large).unwrap().unwrap();
        let retained = decoder.decompress(&input, 1024 * 1024).unwrap();
        let small = vec![b'B'; 32];
        let input = encoder.compress(&small).unwrap().unwrap();

        let decoded = decoder.decompress(&input, small.len()).unwrap();

        assert_eq!(decoded.as_ref(), small);
        assert_eq!(retained.as_ref(), large);
    }
}

#[test]
fn decompression_rejects_reserved_block_type() {
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    // BTYPE=11 is reserved and cannot be repaired by the appended trailer.
    let result = decoder.decompress(&[0x07], 1024);

    assert!(matches!(result, Err(Error::Compression(_))));
}

#[test]
fn minimum_backend_window_round_trips() {
    let payload = b"minimum backend window payload ".repeat(64);
    let mut encoder =
        DeflateEncoder::new(sockudo_ws::deflate::DeflateWindowBits::Bits9, true, 6, 0);
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::DeflateWindowBits::Bits9, true);

    let compressed = encoder.compress(&payload).unwrap().unwrap();
    let decompressed = decoder.decompress(&compressed, payload.len()).unwrap();

    assert_eq!(decompressed.as_ref(), payload);
}

#[test]
fn decompression_handles_long_payloads_that_exactly_fill_the_output_capacity() {
    use flate2::{Compress, Compression, FlushCompress};

    // A random prefix keeps the compressed payload large; the run length is
    // searched so the decompressed length is
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

    // A random prefix creates a large payload. The run gets its own final
    // block of a few short codes, and
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
