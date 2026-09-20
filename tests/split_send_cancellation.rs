#![cfg(feature = "tokio-runtime")]

use std::task::Poll;

use bytes::Bytes;
use futures_util::poll;
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn cancelled_partial_send_closes_connection_before_next_frame() {
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (_reader, mut writer) = WebSocketStream::server(io, config).split();
    {
        let send = writer.send(Message::binary(vec![42; 64]));
        tokio::pin!(send);
        assert!(poll!(&mut send).is_pending());
        let mut prefix = [0; 8];
        peer.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix[..2], &[0x82, 64]);
    }
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
        let mut prefix = [0; 8];
        peer.read_exact(&mut prefix).await.unwrap();
    }
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

#[tokio::test]
async fn cancelling_queued_send_keeps_connection_open_and_drops_request() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        // Keep the driver inside a Pong write while the application request is
        // accepted. The cancelled message therefore cannot have reached either
        // the encoder or the transport.
        let (io, mut peer) = tokio::io::duplex(8);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let (mut reader, mut writer) = WebSocketStream::server(io, config).split();

        let mut ping = vec![0x89, 0xc0, 0, 0, 0, 0];
        ping.extend_from_slice(&[b'p'; 64]);
        let (written, first) = tokio::join!(peer.write_all(&ping), reader.next());
        written.unwrap();
        assert!(first.unwrap().unwrap().is_ping());
        tokio::task::yield_now().await;

        {
            let send = writer.send(Message::binary(Bytes::from_static(b"x")));
            tokio::pin!(send);
            assert!(poll!(&mut send).is_pending());
        }
        assert!(!writer.is_closed());

        let mut pong = [0; 66];
        peer.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong[..2], &[0x8a, 64]);

        let mut next = [0; 6];
        let (sent, read) = tokio::join!(
            writer.send(Message::text("next")),
            peer.read_exact(&mut next)
        );
        sent.unwrap();
        read.unwrap();
        assert_eq!(&next, b"\x81\x04next");
        assert!(!writer.is_closed());
    })
    .await
    .expect("queued cancellation recovery must complete");
}
