#![cfg(feature = "permessage-deflate")]

use rstest::rstest;
use sockudo_ws::Error;
use sockudo_ws::deflate::{DeflateDecoder, DeflateEncoder};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn deflate_decoder_remains_send_and_sync() {
    assert_send_sync::<DeflateDecoder>();
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
    output
}

fn finish_message(payload: &[u8], dictionary: Option<&[u8]>) -> Vec<u8> {
    let mut output = finish_deflate(payload, dictionary);
    // RFC 7692 retains the header byte of the following empty stored block
    // when a final DEFLATE block is transformed into a message payload.
    output.push(0);
    output
}

#[test]
fn decompression_rejects_invalid_data_after_a_final_block() {
    let payload = vec![b'A'; 1024];
    let mut compressed = finish_message(&payload, None);
    compressed.push(0x07);
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    let result = decoder.decompress(&compressed, payload.len());

    assert!(matches!(result, Err(Error::Compression(_))));
}

#[test]
fn decompression_accepts_a_valid_final_block() {
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    let decoded = decoder
        .decompress(&[0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00], 5)
        .unwrap();

    assert_eq!(decoded.as_ref(), b"Hello");
}

#[test]
fn decompression_accepts_an_empty_final_stored_block() {
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    let decoded = decoder.decompress(&[0x01], 0).unwrap();

    assert!(decoded.is_empty());
}

#[test]
fn decompression_preserves_context_after_a_final_block() {
    let first = vec![b'A'; 1024];
    let second = [vec![b'A'; 768], vec![b'B'; 256]].concat();
    let first_compressed = finish_message(&first, None);
    let second_compressed = finish_message(&second, Some(&first));
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);

    assert_eq!(
        decoder
            .decompress(&first_compressed, first.len())
            .unwrap()
            .as_ref(),
        first
    );
    assert_eq!(
        decoder
            .decompress(&second_compressed, second.len())
            .unwrap()
            .as_ref(),
        second
    );
}

#[test]
fn decompression_accepts_multiple_final_blocks_in_one_message() {
    let first = vec![b'A'; 1024];
    let second = [vec![b'A'; 768], vec![b'B'; 256]].concat();
    let mut compressed = finish_deflate(&first, None);
    compressed.extend_from_slice(&finish_deflate(&second, Some(&first)));
    compressed.push(0);
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);

    let decoded = decoder
        .decompress(&compressed, first.len() + second.len())
        .unwrap();

    assert_eq!(decoded.as_ref(), [first, second].concat());
}

#[test]
fn decompression_rejects_output_over_limit_across_final_streams() {
    let first = vec![b'A'; 1024];
    let second = [vec![b'A'; 768], vec![b'B'; 256]].concat();
    let mut compressed = finish_deflate(&first, None);
    compressed.extend_from_slice(&finish_deflate(&second, Some(&first)));
    compressed.push(0);
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);

    let result = decoder.decompress(&compressed, first.len());

    assert!(matches!(result, Err(Error::MessageTooLarge)));
}

#[test]
fn decompression_accepts_consecutive_final_blocks_without_context_takeover() {
    let payloads = [vec![b'A'; 1024], vec![b'B'; 1024]];
    let compressed = payloads
        .each_ref()
        .map(|payload| finish_message(payload, None));
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, true);

    for (input, expected) in compressed.iter().zip(&payloads) {
        assert_eq!(
            decoder.decompress(input, expected.len()).unwrap().as_ref(),
            expected
        );
    }
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

#[rstest]
#[case::empty(0)]
#[case::bytes_32(32)]
#[case::bytes_1024(1024)]
#[case::bytes_1025(1025)]
#[case::bytes_5120(5120)]
#[case::bytes_65536(65536)]
fn decompression_accepts_exact_limits_across_output_growth(#[case] size: usize) {
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

#[rstest]
#[case::takeover(false)]
#[case::no_takeover(true)]
fn decompression_reuses_input_without_retaining_previous_message_bytes(
    #[case] no_context_takeover: bool,
) {
    let mut encoder = flate2::Compress::new(flate2::Compression::new(6), false);
    let mut decoder =
        DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, no_context_takeover);
    let mut state = 0x1234_5678_9abc_def0u64;
    for size in [32, 65536, 1, 4096, 0, 256, 32] {
        let payload: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        if no_context_takeover {
            encoder.reset();
        }
        if size == 0 {
            // Repeated Sync flush need not emit bytes; encode an empty stored block.
            assert!(decoder.decompress(&[0], 0).unwrap().is_empty());
            continue;
        }
        let mut encoded = vec![0; size * 2 + 128];
        let before_in = encoder.total_in();
        let before_out = encoder.total_out();
        encoder
            .compress(&payload, &mut encoded, flate2::FlushCompress::Sync)
            .unwrap();
        assert_eq!(encoder.total_in() - before_in, size as u64);
        encoded.truncate((encoder.total_out() - before_out) as usize);
        assert!(encoded.ends_with(&[0, 0, 255, 255]));
        encoded.truncate(encoded.len() - 4);
        assert_eq!(
            decoder.decompress(&encoded, size).unwrap().as_ref(),
            payload
        );
    }
}

#[test]
fn decoder_reset_allows_reuse_after_a_rejected_message() {
    let mut decoder = DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false);
    assert!(decoder.decompress(&[0x07; 4096], 1024).is_err());
    decoder.reset();
    let encoded = finish_message(b"valid after reset", None);
    assert_eq!(
        decoder.decompress(&encoded, 17).unwrap().as_ref(),
        b"valid after reset"
    );
}
