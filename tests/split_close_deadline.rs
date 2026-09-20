#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use futures_util::poll;
use sockudo_ws::{Config, Error, WebSocketStream};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn zero_close_timeout_still_attempts_the_close_frame() {
    tokio::time::timeout(Duration::from_secs(1), async {
        let (io, mut peer) = tokio::io::duplex(4096);
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .close_timeout(0)
            .build();
        let (_reader, mut writer) = WebSocketStream::server(io, config).split();

        writer.close(1000, "bye").await.unwrap();
        let mut first = [0; 1];
        peer.read_exact(&mut first).await.unwrap();
        assert_eq!(first[0], 0x88);
    })
    .await
    .expect("an immediately writable Close must finish its best-effort attempt");
}

#[tokio::test]
async fn cancelling_an_accepted_close_still_finishes_the_close_frame() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (io, mut peer) = tokio::io::duplex(8);
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .close_timeout(1)
            .build();
        let (_reader, mut writer) = WebSocketStream::server(io, config).split();
        let reason = "x".repeat(80);

        {
            let close = writer.close(1000, &reason);
            tokio::pin!(close);
            assert!(poll!(&mut close).is_pending());
        }
        assert!(writer.is_closed());

        let mut frame = [0; 84];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame[..4], &[0x88, 82, 0x03, 0xe8]);
        assert_eq!(&frame[4..], reason.as_bytes());
    })
    .await
    .expect("an accepted Close must outlive its caller");
}

#[tokio::test(start_paused = true)]
async fn application_close_write_uses_the_closing_budget() {
    let (io, _peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (_reader, mut writer) = WebSocketStream::server(io, config).split();
    let reason = "x".repeat(80);
    let close = writer.close(1000, &reason);
    tokio::pin!(close);
    assert!(poll!(&mut close).is_pending());
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), close)
        .await
        .expect("a blocked Close must not outlive the closing budget");

    assert!(matches!(result, Err(Error::ConnectionClosed)));
}

#[tokio::test(start_paused = true)]
async fn application_close_budget_includes_a_blocked_control_write() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let sending = tokio::spawn(async move {
        let mut ping = vec![0x89, 64];
        ping.extend_from_slice(&[b'p'; 64]);
        peer.write_all(&ping).await.unwrap();
        peer
    });
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    let mut peer = sending.await.unwrap();
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();
    let close = writer.close(1000, "done");
    tokio::pin!(close);
    assert!(poll!(&mut close).is_pending());
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), close)
        .await
        .expect("waiting behind a blocked Pong must use the closing budget");

    assert!(matches!(result, Err(Error::ConnectionClosed)));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test(start_paused = true)]
async fn compressed_close_budget_includes_a_blocked_control_write() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::DeflateConfig::default(),
    )
    .split();
    let sending = tokio::spawn(async move {
        let mut ping = vec![0x89, 64];
        ping.extend_from_slice(&[b'p'; 64]);
        peer.write_all(&ping).await.unwrap();
        peer
    });
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    let mut peer = sending.await.unwrap();
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();
    let close = writer.close(1000, "done");
    tokio::pin!(close);
    assert!(poll!(&mut close).is_pending());
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), close)
        .await
        .expect("waiting behind a blocked Pong must use the closing budget");

    assert!(matches!(result, Err(Error::ConnectionClosed)));
}
