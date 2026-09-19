#![cfg(feature = "tokio-runtime")]

use futures_util::SinkExt;
use sockudo_ws::{Config, Error, Message, WebSocketStream};

#[tokio::test]
async fn exceeding_pending_byte_limit_terminates_unified_writes() {
    let (io, _peer) = tokio::io::duplex(1024);
    let mut ws = WebSocketStream::server(io, Config::builder().max_backpressure(16).build());
    ws.feed(Message::binary(vec![1; 8])).await.unwrap();
    let result = ws.feed(Message::binary(vec![2; 8])).await;
    assert!(matches!(result, Err(Error::BufferFull)));
    assert!(matches!(ws.flush().await, Err(Error::ConnectionClosed)));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn exceeding_pending_byte_limit_terminates_compressed_writes() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, _peer) = tokio::io::duplex(1024);
    let mut ws = CompressedWebSocketStream::server(
        io,
        Config::builder().max_backpressure(16).build(),
        DeflateConfig::default(),
    );
    ws.feed(Message::Ping(bytes::Bytes::from_static(&[1; 8])))
        .await
        .unwrap();
    let result = ws
        .feed(Message::Ping(bytes::Bytes::from_static(&[2; 8])))
        .await;
    assert!(matches!(result, Err(Error::BufferFull)));
    assert!(matches!(ws.flush().await, Err(Error::ConnectionClosed)));
}

#[tokio::test]
async fn oversized_split_frame_fails_before_writing() {
    let (io, _peer) = tokio::io::duplex(1024);
    let (_reader, mut writer) =
        WebSocketStream::server(io, Config::builder().max_backpressure(16).build()).split();
    let result = writer.send(Message::binary(vec![1; 32])).await;
    assert!(matches!(result, Err(Error::BufferFull)));
    assert!(writer.is_closed());
}

#[tokio::test]
async fn zero_copy_payloads_count_toward_the_encoded_limit() {
    let size = sockudo_ws::cork::ZERO_COPY_MIN;
    let (io, mut peer) = tokio::io::duplex(1024);
    let mut ws = WebSocketStream::server(io, Config::builder().max_backpressure(size).build());
    assert!(matches!(
        ws.feed(Message::binary(vec![1; size])).await,
        Err(Error::BufferFull)
    ));
    assert!(matches!(ws.flush().await, Err(Error::ConnectionClosed)));
    let mut byte = [0];
    use tokio::io::AsyncReadExt;
    let read = std::pin::pin!(peer.read(&mut byte));
    assert!(futures_util::poll!(read).is_pending());
}

#[tokio::test]
async fn an_encoded_frame_exactly_at_the_limit_is_accepted() {
    let (io, mut peer) = tokio::io::duplex(64);
    let mut ws = WebSocketStream::server(io, Config::builder().max_backpressure(10).build());
    ws.send(Message::binary(vec![1; 8])).await.unwrap();
    let mut wire = [0; 10];
    use tokio::io::AsyncReadExt;
    peer.read_exact(&mut wire).await.unwrap();
    assert_eq!(&wire[..2], &[0x82, 8]);
    assert_eq!(&wire[2..], &[1; 8]);
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn oversized_compressed_split_frame_is_rejected() {
    let (io, _peer) = tokio::io::duplex(64);
    let (_reader, mut writer) = sockudo_ws::CompressedWebSocketStream::server(
        io,
        Config::builder().max_backpressure(8).build(),
        sockudo_ws::DeflateConfig::default(),
    )
    .split();
    assert!(matches!(
        writer
            .send(Message::Ping(bytes::Bytes::from_static(b"12345678")))
            .await,
        Err(Error::BufferFull)
    ));
    assert!(writer.is_closed());
}

#[test]
fn encoded_buffer_limit_is_a_terminal_error() {
    assert!(Error::BufferFull.is_fatal());
    assert!(!Error::BufferFull.is_recoverable());
}
