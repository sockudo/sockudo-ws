#![cfg(feature = "permessage-deflate")]

use sockudo_ws::deflate::{DeflateEncoder, MAX_WINDOW_BITS};

fn incompressible(len: usize) -> Vec<u8> {
    // Fixed input that expands beyond the encoder's initial output capacity.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..len)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

fn decode(encoded: &[u8], expected_len: usize) -> Vec<u8> {
    let mut input = encoded.to_vec();
    input.extend_from_slice(&[0, 0, 255, 255]);
    let mut peer = flate2::Decompress::new(false);
    let mut output = vec![0; expected_len + 64];
    peer.decompress(&input, &mut output, flate2::FlushDecompress::Sync)
        .unwrap();
    output.truncate(peer.total_out() as usize);
    output
}

#[test]
fn takeover_flush_preserves_large_incompressible_payloads() {
    for len in [192 * 1024, 1024 * 1024] {
        let payload = incompressible(len);
        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, false, 6, 0);

        let encoded = encoder.compress(&payload).unwrap().unwrap();

        assert_eq!(decode(&encoded, len), payload);
    }
}

#[test]
fn discarded_incompressible_output_does_not_leak_into_next_message() {
    for len in [192 * 1024, 1024 * 1024] {
        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 0);
        assert!(encoder.compress(&incompressible(len)).unwrap().is_none());
        let payload = b"repeat one ".repeat(1000);

        let encoded = encoder.compress(&payload).unwrap().unwrap();

        assert_eq!(decode(&encoded, payload.len()), payload);
    }
}

#[test]
fn no_takeover_messages_decode_with_fresh_peer_contexts() {
    for level in [1, 6, 9] {
        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, level, 32);
        let payloads = [
            b"repeat one ".repeat(1000),
            Vec::new(),
            b"tiny".to_vec(),
            incompressible(192 * 1024),
            b"repeat two ".repeat(300),
            b"repeat one ".repeat(1000),
            b"large message ".repeat(100_000),
            b"repeat two ".repeat(8),
        ];
        for payload in &payloads {
            if let Some(encoded) = encoder.compress(payload).unwrap() {
                assert_eq!(decode(&encoded, payload.len()), *payload);
            }
        }
    }
}
