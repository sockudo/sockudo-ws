#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::poll;
use sockudo_ws::protocol::Protocol;
use sockudo_ws::{Config, Error, Message, Role, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[tokio::test(start_paused = true)]
async fn idle_timeout_interrupts_an_application_holding_the_sink() {
    let (io, _peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::server(io, config).split();
    let send = writer.send(Message::binary(vec![42; 64]));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let terminal = tokio::time::timeout(Duration::from_secs(1), reader.next()).await;
    assert!(matches!(terminal, Ok(Some(Err(Error::IdleTimeout)))));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn automatic_ping_waiting_for_sink_does_not_hide_idle_timeout() {
    let (io, _peer) = tokio::io::duplex(8);
    let config = Config::builder().ping_interval(1).idle_timeout(2).build();
    let (mut reader, mut writer) = WebSocketStream::server(io, config).split();
    let send = writer.send(Message::binary(vec![42; 64]));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let terminal = tokio::time::timeout(Duration::from_secs(1), reader.next()).await;
    assert!(matches!(terminal, Ok(Some(Err(Error::IdleTimeout)))));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn peer_ping_waiting_for_sink_does_not_hide_idle_timeout() {
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::server(io, config).split();
    let send = writer.send(Message::binary(vec![42; 64]));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    let mut wire = BytesMut::new();
    Protocol::new(Role::Client, 1024, 1024)
        .encode_message(&Message::Ping(Bytes::from_static(b"p")), &mut wire)
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    assert!(matches!(reader.next().await, Some(Ok(Message::Ping(_)))));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let terminal = tokio::time::timeout(Duration::from_secs(1), reader.next()).await;
    assert!(matches!(terminal, Ok(Some(Err(Error::IdleTimeout)))));
}

#[tokio::test(start_paused = true)]
async fn partially_written_automatic_ping_observes_idle_timeout() {
    let (io, _peer) = tokio::io::duplex(8);
    let config = Config::builder().ping_interval(1).idle_timeout(2).build();
    let (mut reader, _writer) = WebSocketStream::server(io, config).split();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let terminal = tokio::time::timeout(Duration::from_secs(1), reader.next()).await;
    assert!(matches!(terminal, Ok(Some(Err(Error::IdleTimeout)))));
}

#[tokio::test(start_paused = true)]
async fn inbound_data_postpones_idle_expiry_during_blocked_send() {
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::server(io, config).split();
    let send = writer.send(Message::binary(vec![42; 64]));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(800)).await;
    let mut wire = BytesMut::new();
    Protocol::new(Role::Client, 1024, 1024)
        .encode_message(&Message::binary(b"d".to_vec()), &mut wire)
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    assert!(matches!(reader.next().await, Some(Ok(Message::Binary(_)))));
    tokio::time::advance(Duration::from_millis(500)).await;
    tokio::task::yield_now().await;
    assert!(!reader.is_closed());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), reader.next()).await,
        Ok(Some(Err(Error::IdleTimeout)))
    ));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test(start_paused = true)]
async fn compressed_writer_observes_idle_timeout_while_holding_sink() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, _peer) = tokio::io::duplex(8);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) =
        CompressedWebSocketStream::server(io, config, DeflateConfig::default()).split();
    let send = writer.send(Message::Ping(Bytes::from(vec![42; 64])));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), reader.next()).await,
        Ok(Some(Err(Error::IdleTimeout)))
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap()
            .is_err()
    );
}

struct FlushGate {
    inner: tokio::io::DuplexStream,
    blocked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}

impl tokio::io::AsyncRead for FlushGate {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for FlushGate {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.blocked.load(std::sync::atomic::Ordering::Relaxed) {
            *self.waker.lock().unwrap() = Some(cx.waker().clone());
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(Ok(()))
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn pong_received_before_ping_flush_completes_is_preserved() {
    use tokio::io::AsyncReadExt;
    let (io, mut peer) = tokio::io::duplex(128);
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let waker = std::sync::Arc::new(std::sync::Mutex::new(None));
    let gate = FlushGate {
        inner: io,
        blocked: blocked.clone(),
        waker: waker.clone(),
    };
    let config = Config::builder()
        .ping_interval(10)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut reader, _writer) = WebSocketStream::server(gate, config).split();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut ping = [0; 10];
    peer.read_exact(&mut ping).await.unwrap();
    assert_eq!(&ping[..2], &[0x89, 8]);
    let mut wire = BytesMut::new();
    Protocol::new(Role::Client, 1024, 1024)
        .encode_message(
            &Message::Pong(Bytes::copy_from_slice(&ping[2..])),
            &mut wire,
        )
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    assert!(matches!(reader.next().await, Some(Ok(Message::Pong(_)))));
    tokio::task::yield_now().await;
    blocked.store(false, std::sync::atomic::Ordering::Relaxed);
    waker.lock().unwrap().take().unwrap().wake();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(
        !reader.is_closed(),
        "the matching Pong must prevent a false timeout"
    );
}

#[tokio::test(start_paused = true)]
async fn peer_close_waiting_for_sink_is_bounded() {
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::server(io, config).split();
    let send = writer.send(Message::binary(vec![42; 64]));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    let mut wire = BytesMut::new();
    Protocol::new(Role::Client, 1024, 1024)
        .encode_message(&Message::Close(None), &mut wire)
        .unwrap();
    peer.write_all(&wire).await.unwrap();
    assert!(matches!(reader.next().await, Some(Ok(Message::Close(_)))));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap()
            .is_err()
    );
}
