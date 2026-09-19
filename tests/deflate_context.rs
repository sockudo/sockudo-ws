#![cfg(feature = "permessage-deflate")]

use bytes::{Bytes, BytesMut};
use sockudo_ws::deflate::{DeflateConfig, DeflateDecoder, DeflateEncoder};
use sockudo_ws::protocol::CompressedWriterProtocol;
use sockudo_ws::{CompressedProtocol, Message};

#[test]
fn incompressible_messages_keep_encoder_and_decoder_history_in_sync() {
    for no_context_takeover in [false, true] {
        let mut encoder = DeflateEncoder::new(
            sockudo_ws::deflate::MAX_WINDOW_BITS,
            no_context_takeover,
            6,
            0,
        );
        let mut decoder =
            DeflateDecoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, no_context_takeover);
        let incompressible: Vec<u8> = (0..=255).collect();
        let first = encoder.compress(&incompressible).unwrap();
        assert_eq!(first.is_none(), no_context_takeover);
        if let Some(compressed) = first {
            assert_eq!(
                decoder
                    .decompress(&compressed, incompressible.len())
                    .unwrap()
                    .as_ref(),
                incompressible
            );
        }
        let payload = incompressible.repeat(2);

        let compressed = encoder.compress(&payload).unwrap().unwrap();
        let decoded = decoder.decompress(&compressed, payload.len()).unwrap();

        assert_eq!(decoded.as_ref(), payload);
    }
}

#[test]
fn alternating_compressed_and_plain_frames_preserve_payloads() {
    for split_writer in [false, true] {
        let config = DeflateConfig {
            compression_threshold: 64,
            ..DeflateConfig::default()
        };
        let mut sender = CompressedProtocol::server(4096, 4096, config.clone());
        let mut writer = CompressedWriterProtocol::server(&config);
        let mut receiver = CompressedProtocol::client(4096, 4096, config);
        let incompressible: Vec<u8> = (0..=255).collect();
        let payloads = [
            Bytes::from(vec![b'A'; 1024]),
            Bytes::from(incompressible.clone()),
            Bytes::from(incompressible.repeat(2)),
            Bytes::from_static(b"below threshold"),
            Bytes::from(vec![b'A'; 1024]),
            Bytes::from(vec![b'A'; 1024]),
        ];
        let mut wire = BytesMut::new();
        for (index, payload) in payloads.iter().enumerate() {
            let start = wire.len();
            let message = Message::Binary(payload.clone());
            if split_writer {
                writer.encode_message(&message, &mut wire).unwrap();
            } else {
                sender.encode_message(&message, &mut wire).unwrap();
            }
            assert_eq!(wire[start] & 0x40 != 0, index != 3);
        }

        let decoded = receiver.process(&mut wire).unwrap();

        assert!(wire.is_empty());
        assert_eq!(decoded.len(), payloads.len());
        for (actual, expected) in decoded.iter().zip(&payloads) {
            assert_eq!(actual.as_bytes(), expected);
        }
    }
}
