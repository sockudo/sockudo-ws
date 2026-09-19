#![cfg(feature = "tokio-runtime")]

use bytes::BytesMut;
use futures_util::StreamExt;
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn unified_reader_preserves_order_and_owned_payloads_between_batches() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let mut ws = WebSocketStream::client(io, Config::default());
    let mut received = Vec::new();
    for batch in 0u8..3 {
        let mut wire = BytesMut::new();
        for index in 0u8..32 {
            encode_frame(&mut wire, OpCode::Binary, &[batch, index], true, None);
        }
        peer.write_all(&wire).await.unwrap();
        for _ in 0..32 {
            received.push(ws.next().await.unwrap().unwrap().into_bytes());
        }
    }
    let actual: Vec<_> = received.iter().map(|bytes| bytes.as_ref()).collect();
    let expected: Vec<_> = (0u8..3)
        .flat_map(|batch| (0u8..32).map(move |index| [batch, index]))
        .collect();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn split_reader_preserves_order_and_owned_payloads_between_batches() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let ws = WebSocketStream::client(io, Config::default());
    let (mut reader, _writer) = ws.split();
    let mut received = Vec::new();
    for batch in 0u8..3 {
        let mut wire = BytesMut::new();
        for index in 0u8..32 {
            encode_frame(&mut wire, OpCode::Binary, &[batch, index], true, None);
        }
        peer.write_all(&wire).await.unwrap();
        for _ in 0..32 {
            received.push(reader.next().await.unwrap().unwrap().into_bytes());
        }
    }
    let actual: Vec<_> = received.iter().map(|bytes| bytes.as_ref()).collect();
    let expected: Vec<_> = (0u8..3)
        .flat_map(|batch| (0u8..32).map(move |index| [batch, index]))
        .collect();
    assert_eq!(actual, expected);
}
