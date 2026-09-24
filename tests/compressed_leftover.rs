#![cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, poll};
use sockudo_ws::deflate::{DeflateEncoder, MAX_WINDOW_BITS};
use sockudo_ws::frame::encode_frame_with_rsv;
use sockudo_ws::{CompressedWebSocketStream, Config, DeflateConfig, Error, OpCode};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn client_delivers_handshake_leftover_without_another_read() {
    let (io, _peer) = tokio::io::duplex(128);
    let mut ws = CompressedWebSocketStream::client_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(Bytes::from_static(b"\x82\x01a")),
    );
    let result = poll!(std::pin::pin!(ws.next()));
    assert!(
        matches!(result, std::task::Poll::Ready(Some(Ok(message))) if message.as_bytes() == b"a")
    );
}

#[tokio::test]
async fn server_split_delivers_masked_handshake_leftover() {
    let (io, _peer) = tokio::io::duplex(128);
    let ws = CompressedWebSocketStream::server_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(Bytes::from_static(b"\x82\x81\0\0\0\0a")),
    );
    let (mut reader, _writer) = ws.split();
    let result = poll!(std::pin::pin!(reader.next()));
    assert!(
        matches!(result, std::task::Poll::Ready(Some(Ok(message))) if message.as_bytes() == b"a")
    );
}

#[tokio::test]
async fn unified_reader_completes_a_partial_payload_from_handshake_leftover() {
    let (io, mut peer) = tokio::io::duplex(128);
    let mut ws = CompressedWebSocketStream::client_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(Bytes::from_static(b"\x82\x03a")),
    );
    assert!(poll!(std::pin::pin!(ws.next())).is_pending());
    peer.write_all(b"bc").await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"abc");
}

#[tokio::test]
async fn client_decompresses_a_handshake_leftover_frame() {
    let payload = b"compressed data compressed data ".repeat(8);
    let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, true, 6, 0);
    let compressed = encoder.compress(&payload).unwrap().unwrap();
    let mut wire = BytesMut::new();
    encode_frame_with_rsv(&mut wire, OpCode::Binary, &compressed, true, None, true);

    let (io, _peer) = tokio::io::duplex(128);
    let mut ws = CompressedWebSocketStream::client_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(wire.freeze()),
    );

    let result = poll!(std::pin::pin!(ws.next()));
    assert!(
        matches!(result, std::task::Poll::Ready(Some(Ok(message))) if message.as_bytes() == payload)
    );
}

#[tokio::test]
async fn split_reader_completes_a_partial_handshake_leftover_frame() {
    let (io, mut peer) = tokio::io::duplex(128);
    let ws = CompressedWebSocketStream::server_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(Bytes::from_static(b"\x82\x83\0\0\0\0a")),
    );
    let (mut reader, _writer) = ws.split();

    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
    peer.write_all(b"bc").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
}

#[tokio::test]
async fn unified_reader_delivers_valid_leftover_before_parse_error() {
    let mut wire = BytesMut::new();
    encode_frame_with_rsv(&mut wire, OpCode::Binary, b"accepted", true, None, false);
    encode_frame_with_rsv(&mut wire, OpCode::Ping, b"bad", true, None, true);
    let (io, _peer) = tokio::io::duplex(128);
    let mut ws = CompressedWebSocketStream::client_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(wire.freeze()),
    );

    let message = ws.next().await.unwrap().unwrap();
    assert_eq!(message.as_bytes(), b"accepted");
    assert!(matches!(
        ws.next().await,
        Some(Err(Error::Protocol(
            "RSV1 on control or continuation frame"
        )))
    ));
}

#[tokio::test]
async fn split_reader_delivers_valid_leftover_before_parse_error() {
    let mut wire = BytesMut::new();
    encode_frame_with_rsv(&mut wire, OpCode::Binary, b"accepted", true, None, false);
    encode_frame_with_rsv(&mut wire, OpCode::Ping, b"bad", true, None, true);
    let (io, _peer) = tokio::io::duplex(128);
    let ws = CompressedWebSocketStream::client_with_leftover(
        io,
        Config::default(),
        DeflateConfig::default(),
        Some(wire.freeze()),
    );
    let (mut reader, _writer) = ws.split();

    let message = reader.next().await.unwrap().unwrap();
    assert_eq!(message.as_bytes(), b"accepted");
    assert!(matches!(
        reader.next().await,
        Some(Err(Error::Protocol(
            "RSV1 on control or continuation frame"
        )))
    ));
}
