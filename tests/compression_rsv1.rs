#![cfg(feature = "permessage-deflate")]

use bytes::BytesMut;
use sockudo_ws::frame::{FrameParser, encode_frame_with_rsv};
use sockudo_ws::{CompressedProtocol, DeflateConfig, Error, Message, OpCode, Role};

fn protocol(role: Role) -> CompressedProtocol {
    match role {
        Role::Client => CompressedProtocol::client(1024, 1024, DeflateConfig::default()),
        Role::Server => CompressedProtocol::server(1024, 1024, DeflateConfig::default()),
    }
}

#[test]
fn compressed_readers_reject_rsv1_on_control_and_continuation_frames() {
    for role in [Role::Client, Role::Server] {
        let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
        for opcode in [
            OpCode::Continuation,
            OpCode::Close,
            OpCode::Ping,
            OpCode::Pong,
        ] {
            let mut wire = BytesMut::new();
            if opcode == OpCode::Continuation {
                encode_frame_with_rsv(&mut wire, OpCode::Text, b"x", false, mask, false);
            }
            encode_frame_with_rsv(&mut wire, opcode, b"", true, mask, true);
            let (mut reader, _) = protocol(role).split(1024, 1024);
            assert!(matches!(
                reader.process(&mut wire.clone()),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));
            assert!(matches!(
                protocol(role).process(&mut wire),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));
        }
    }
}

#[test]
fn compressed_readers_accept_rsv1_on_a_fragmented_data_message() {
    use sockudo_ws::deflate::{DeflateEncoder, MAX_WINDOW_BITS};

    let payload = b"compressed data compressed data ".repeat(8);
    let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 0);
    let compressed = encoder.compress(&payload).unwrap().unwrap();
    let midpoint = compressed.len() / 2;
    assert!(midpoint > 0);

    for role in [Role::Client, Role::Server] {
        for opcode in [OpCode::Text, OpCode::Binary] {
            let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
            let mut wire = BytesMut::new();
            encode_frame_with_rsv(
                &mut wire,
                opcode,
                &compressed[..midpoint],
                false,
                mask,
                true,
            );
            encode_frame_with_rsv(&mut wire, OpCode::Ping, b"p", true, mask, false);
            encode_frame_with_rsv(
                &mut wire,
                OpCode::Continuation,
                &compressed[midpoint..],
                true,
                mask,
                false,
            );
            encode_frame_with_rsv(&mut wire, OpCode::Text, b"plain", true, mask, false);

            let (mut reader, _) = protocol(role).split(1024, 1024);
            for messages in [
                reader.process(&mut wire.clone()).unwrap(),
                protocol(role).process(&mut wire).unwrap(),
            ] {
                assert_eq!(messages.len(), 3);
                assert!(matches!(&messages[0], Message::Ping(bytes) if bytes.as_ref() == b"p"));
                assert!(
                    matches!((&messages[1], opcode), (Message::Text(bytes), OpCode::Text) | (Message::Binary(bytes), OpCode::Binary) if bytes.as_ref() == payload)
                );
                assert!(matches!(&messages[2], Message::Text(bytes) if bytes.as_ref() == b"plain"));
            }
        }
    }
}

#[test]
fn compressed_readers_keep_messages_before_an_invalid_rsv1_frame() {
    for role in [Role::Client, Role::Server] {
        let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
        let mut wire = BytesMut::new();
        encode_frame_with_rsv(&mut wire, OpCode::Binary, b"accepted", true, mask, false);
        encode_frame_with_rsv(&mut wire, OpCode::Ping, b"bad", true, mask, true);

        let (mut reader, _) = protocol(role).split(1024, 1024);
        let mut messages = Vec::new();
        assert!(matches!(
            reader.process_into(&mut wire.clone(), &mut messages),
            Err(Error::Protocol("RSV1 on control or continuation frame"))
        ));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].as_bytes(), b"accepted");

        assert!(matches!(
            protocol(role).process_into(&mut wire, &mut messages),
            Err(Error::Protocol("RSV1 on control or continuation frame"))
        ));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].as_bytes(), b"accepted");
    }
}

#[test]
fn compressed_readers_reject_rsv1_after_a_partial_frame() {
    for role in [Role::Client, Role::Server] {
        let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
        let mut wire = BytesMut::new();
        encode_frame_with_rsv(&mut wire, OpCode::Ping, b"bad", true, mask, true);
        let remainder = wire.split_off(1);

        let (mut reader, _) = protocol(role).split(1024, 1024);
        let mut input = wire.clone();
        assert!(reader.process(&mut input).unwrap().is_empty());
        input.extend_from_slice(&remainder);
        assert!(matches!(
            reader.process(&mut input),
            Err(Error::Protocol("RSV1 on control or continuation frame"))
        ));

        let mut unified = protocol(role);
        assert!(unified.process(&mut wire).unwrap().is_empty());
        wire.extend_from_slice(&remainder);
        assert!(matches!(
            unified.process(&mut wire),
            Err(Error::Protocol("RSV1 on control or continuation frame"))
        ));
    }
}

#[test]
fn compressed_readers_reject_invalid_rsv1_from_the_base_header() {
    for role in [Role::Client, Role::Server] {
        let masked = role == Role::Server;
        for opcode in [OpCode::Continuation, OpCode::Ping] {
            let payload_len = if opcode == OpCode::Ping { 125 } else { 126 };
            let mut wire = BytesMut::from(
                &[
                    0xC0 | opcode as u8,
                    payload_len | if masked { 0x80 } else { 0 },
                ][..],
            );

            let mut parser = FrameParser::with_compression(1024, masked);
            assert!(matches!(
                parser.parse(&mut wire.clone()),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));

            let (mut reader, _) = protocol(role).split(1024, 1024);
            assert!(matches!(
                reader.process(&mut wire.clone()),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));
            assert!(matches!(
                protocol(role).process(&mut wire),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));
        }
    }
}
