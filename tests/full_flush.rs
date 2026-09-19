#![cfg(feature = "permessage-deflate")]
use sockudo_ws::deflate::DeflateEncoder;
#[test]
fn no_takeover_messages_decode_with_fresh_peer_contexts() {
    let mut encoder = DeflateEncoder::new(15, true, 6, 0);
    for payload in [
        b"repeat one ".repeat(1000),
        b"repeat two ".repeat(300),
        b"repeat one ".repeat(1000),
    ] {
        let mut encoded = encoder.compress(&payload).unwrap().unwrap().to_vec();
        encoded.extend_from_slice(&[0, 0, 255, 255]);
        let mut peer = flate2::Decompress::new(false);
        let mut decoded = vec![0; payload.len() + 64];
        peer.decompress(&encoded, &mut decoded, flate2::FlushDecompress::Sync)
            .unwrap();
        decoded.truncate(peer.total_out() as usize);
        assert_eq!(decoded, payload);
    }
}
