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
        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, level, 0);
        let payloads = [
            (b"repeat one ".repeat(1000), true),
            (Vec::new(), false),
            (Vec::new(), false),
            (b"tiny".to_vec(), false),
            (incompressible(192 * 1024), false),
            (b"repeat two ".repeat(300), true),
            (b"repeat one ".repeat(1000), true),
            (b"large message ".repeat(100_000), true),
            (b"repeat two ".repeat(8), true),
        ];
        for (payload, compressed) in &payloads {
            let encoded = encoder.compress(payload).unwrap();
            assert_eq!(encoded.is_some(), *compressed);
            if let Some(encoded) = encoded {
                assert_eq!(decode(&encoded, payload.len()), *payload);
            }
        }
    }
}

#[test]
fn no_takeover_clears_history_after_multi_call_flush() {
    let first = incompressible(1024 * 1024);
    // Reuse the preceding message's tail so a missing history reset creates
    // cross-message references that a fresh decoder cannot resolve.
    let next = first[first.len() - 1024..].repeat(4);
    for level in [1, 6, 9] {
        // Match the public encoder's initial capacity and prove this fixture
        // consumes all input while output is full: another flush call is required.
        let mut probe = flate2::Compress::new(flate2::Compression::new(level), false);
        let mut output = vec![0; first.len() + 64];
        probe
            .compress(&first, &mut output, flate2::FlushCompress::Full)
            .unwrap();
        assert_eq!(probe.total_in(), first.len() as u64);
        assert_eq!(probe.total_out(), output.len() as u64);

        let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, level, 0);
        assert!(encoder.compress(&first).unwrap().is_none());

        let encoded = encoder.compress(&next).unwrap().unwrap();

        assert_eq!(decode(&encoded, next.len()), next);
    }
}

#[test]
fn no_takeover_remains_independent_after_threshold_skips() {
    let payload = b"repeat one ".repeat(100);
    let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 32);
    assert!(encoder.compress(&payload).unwrap().is_some());
    assert!(encoder.compress(b"").unwrap().is_none());
    assert!(encoder.compress(b"repeat one ").unwrap().is_none());

    let encoded = encoder.compress(&payload).unwrap().unwrap();

    assert_eq!(decode(&encoded, payload.len()), payload);
}
