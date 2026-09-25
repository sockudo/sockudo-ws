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
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap(),
        Err(Error::IdleTimeout)
    ));
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
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap(),
        Err(Error::IdleTimeout)
    ));
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
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap(),
        Err(Error::IdleTimeout)
    ));
}

#[tokio::test(start_paused = true)]
async fn blocked_send_reports_the_pong_timeout_cause() {
    use tokio::io::AsyncReadExt;

    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut reader, mut writer) = WebSocketStream::server(io, config).split();

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let mut ping = [0; 10];
    peer.read_exact(&mut ping).await.unwrap();
    assert_eq!(&ping[..2], &[0x89, 8]);
    tokio::task::yield_now().await;

    let send = writer.send(Message::binary(vec![42; 64]));
    tokio::pin!(send);
    assert!(poll!(&mut send).is_pending());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;

    assert!(matches!(
        reader.next().await,
        Some(Err(Error::HeartbeatTimeout))
    ));
    assert!(matches!(send.await, Err(Error::HeartbeatTimeout)));
}

struct FlushGate {
    inner: tokio::io::DuplexStream,
    blocked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    shutdowns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
        self.shutdowns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_flush_reports_the_idle_timeout_cause() {
    let (io, _peer) = tokio::io::duplex(128);
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let gate = FlushGate {
        inner: io,
        blocked,
        waker: std::sync::Arc::new(std::sync::Mutex::new(None)),
        shutdowns: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::server(gate, config).split();
    let flush = writer.flush();
    tokio::pin!(flush);
    assert!(poll!(&mut flush).is_pending());

    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
    assert!(matches!(flush.await, Err(Error::IdleTimeout)));
}

#[tokio::test(start_paused = true)]
async fn timeout_error_waits_for_the_close_write() {
    use tokio::io::AsyncReadExt;

    let (io, mut peer) = tokio::io::duplex(128);
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let waker = std::sync::Arc::new(std::sync::Mutex::new(None));
    let gate = FlushGate {
        inner: io,
        blocked: blocked.clone(),
        waker: waker.clone(),
        shutdowns: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(1)
        .close_timeout(5)
        .build();
    let (mut reader, _writer) = WebSocketStream::server(gate, config).split();
    let terminal = reader.next();
    tokio::pin!(terminal);
    assert!(poll!(&mut terminal).is_pending());

    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(waker.lock().unwrap().is_some());
    assert!(poll!(&mut terminal).is_pending());

    let mut close_header = [0; 2];
    peer.read_exact(&mut close_header).await.unwrap();
    assert_eq!(close_header[0], 0x88);
    blocked.store(false, std::sync::atomic::Ordering::Relaxed);
    waker.lock().unwrap().take().unwrap().wake();

    assert!(matches!(terminal.await, Some(Err(Error::IdleTimeout))));
}

#[tokio::test(start_paused = true)]
async fn close_deadline_attempts_shutdown_when_the_sink_is_idle() {
    let (io, _peer) = tokio::io::duplex(128);
    let shutdowns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = FlushGate {
        inner: io,
        blocked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        waker: std::sync::Arc::new(std::sync::Mutex::new(None)),
        shutdowns: shutdowns.clone(),
    };
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (_reader, mut writer) = WebSocketStream::server(gate, config).split();

    writer.close(1000, "").await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;

    assert_eq!(shutdowns.load(std::sync::atomic::Ordering::Relaxed), 1);
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
        shutdowns: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .unwrap(),
        Err(Error::ConnectionClosed)
    ));
}
