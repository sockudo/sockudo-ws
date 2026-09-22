#![cfg(feature = "tokio-runtime")]

use std::task::Poll;

#[cfg(feature = "permessage-deflate")]
use bytes::Bytes;
use futures_util::poll;
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn cancelled_partial_send_closes_connection_before_next_frame() {
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (_reader, mut writer) = WebSocketStream::server(io, config).split();
    {
        let send = writer.send(Message::binary(vec![42; 64]));
        tokio::pin!(send);
        assert!(poll!(&mut send).is_pending());
    }
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();
    assert_eq!(&prefix[..2], &[0x82, 64]);
    assert!(writer.is_closed());
    assert!(matches!(
        writer.send(Message::text("next")).await,
        Err(Error::ConnectionClosed)
    ));
    let mut rest = [0; 1];
    let read = peer.read(&mut rest);
    tokio::pin!(read);
    assert!(!matches!(poll!(&mut read), Poll::Ready(Ok(n)) if n > 0));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn cancelled_compressed_writer_send_closes_connection() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (_reader, mut writer) =
        CompressedWebSocketStream::server(io, config, DeflateConfig::default()).split();
    {
        // Control frames stay uncompressed, ensuring a partial write even with deflate enabled.
        let send = writer.send(Message::Ping(Bytes::from(vec![42; 64])));
        tokio::pin!(send);
        assert!(poll!(&mut send).is_pending());
    }
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();
    assert!(writer.is_closed());
    assert!(matches!(
        writer.send(Message::text("next")).await,
        Err(Error::ConnectionClosed)
    ));
}

#[tokio::test]
async fn dropping_unpolled_send_keeps_connection_open() {
    let (io, _peer) = tokio::io::duplex(128);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (_reader, mut writer) = WebSocketStream::server(io, config).split();
    drop(writer.send(Message::text("unused")));
    writer.send(Message::text("next")).await.unwrap();
    assert!(!writer.is_closed());
}
