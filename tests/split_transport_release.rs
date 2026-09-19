#![cfg(feature = "tokio-runtime")]

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{poll, task::AtomicWaker};
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::Notify;

struct WriteGate {
    remaining: AtomicUsize,
    bytes: Mutex<Vec<u8>>,
    blocked: Notify,
    waker: AtomicWaker,
    dropped: AtomicBool,
    flush_blocked: AtomicBool,
}

impl WriteGate {
    fn release(&self) {
        self.remaining.store(usize::MAX, Ordering::Relaxed);
        self.flush_blocked.store(false, Ordering::Relaxed);
        self.waker.wake();
    }
}

struct GatedIo {
    input: DuplexStream,
    gate: Arc<WriteGate>,
}

impl AsyncRead for GatedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.input).poll_read(cx, buf)
    }
}

impl AsyncWrite for GatedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.gate.waker.register(cx.waker());
        let count = bytes.len().min(self.gate.remaining.load(Ordering::Relaxed));
        if count == 0 {
            self.gate.blocked.notify_one();
            return Poll::Pending;
        }
        self.gate
            .bytes
            .lock()
            .unwrap()
            .extend_from_slice(&bytes[..count]);
        self.gate.remaining.fetch_sub(count, Ordering::Relaxed);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.gate.waker.register(cx.waker());
        if self.gate.flush_blocked.load(Ordering::Relaxed) {
            self.gate.blocked.notify_one();
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Model a transport whose shutdown also needs the blocked output path.
        self.gate.waker.register(cx.waker());
        Poll::Pending
    }
}

impl Drop for GatedIo {
    fn drop(&mut self) {
        self.gate.dropped.store(true, Ordering::Relaxed);
    }
}

fn connection(write_limit: usize) -> (GatedIo, DuplexStream, Arc<WriteGate>) {
    let (input, peer) = tokio::io::duplex(4096);
    let gate = Arc::new(WriteGate {
        remaining: AtomicUsize::new(write_limit),
        bytes: Mutex::new(Vec::new()),
        blocked: Notify::new(),
        waker: AtomicWaker::new(),
        dropped: AtomicBool::new(false),
        flush_blocked: AtomicBool::new(false),
    });
    (
        GatedIo {
            input,
            gate: gate.clone(),
        },
        peer,
        gate,
    )
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn idle_timeout_releases_transport_after_partial_write() {
    let (io, _peer, gate) = connection(3);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let send = tokio::spawn(async move {
        let result = writer.send_text("incomplete frame").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(1001)).await;
    tokio::task::yield_now().await;
    assert!(
        send.is_finished(),
        "idle timeout must interrupt the pending send"
    );
    let (mut writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(
        gate.dropped.load(Ordering::Relaxed),
        "live handles must not retain the timed-out transport"
    );
    assert_eq!(
        gate.bytes.lock().unwrap().len(),
        3,
        "no Close frame may follow a partial frame"
    );
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
    assert!(matches!(
        writer.send_text("late").await,
        Err(Error::IdleTimeout)
    ));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn active_send_timeout_is_retained_for_later_sends() {
    let (io, _peer, gate) = connection(3);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = WebSocketStream::client(io, config).split();
    {
        let mut active = std::pin::pin!(writer.send_text("active"));
        assert!(poll!(active.as_mut()).is_pending());
        gate.blocked.notified().await;
        tokio::time::advance(Duration::from_millis(1001)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            poll!(active.as_mut()),
            Poll::Ready(Err(Error::IdleTimeout))
        ));
    }
    assert!(matches!(
        writer.send_text("later").await,
        Err(Error::IdleTimeout)
    ));
    assert!(gate.dropped.load(Ordering::Relaxed));
    assert_eq!(gate.bytes.lock().unwrap().len(), 3);
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn inbound_data_postpones_idle_timeout_during_blocked_write() {
    let (io, mut peer, gate) = connection(3);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let send = tokio::spawn(async move {
        let result = writer.send_text("active").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    peer.write_all(b"\x82\x01x").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;
    assert!(!send.is_finished());
    tokio::time::advance(Duration::from_millis(401)).await;
    tokio::task::yield_now().await;
    assert!(
        send.is_finished(),
        "postponed idle deadline must still expire"
    );
    let (_writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn due_ping_does_not_hide_idle_expiry_during_blocked_write() {
    let (io, _peer, gate) = connection(3);
    let config = Config::builder().ping_interval(1).idle_timeout(2).build();
    let (_reader, mut writer) = WebSocketStream::client(io, config).split();
    let send = tokio::spawn(async move {
        let result = writer.send_text("active").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(2001)).await;
    tokio::task::yield_now().await;
    assert!(
        send.is_finished(),
        "deferred Ping must not suppress hard idle timeout"
    );
    let (_writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(gate.bytes.lock().unwrap().len(), 3);
}

async fn outstanding_ping() -> (
    sockudo_ws::SplitReader<GatedIo>,
    sockudo_ws::SplitWriter<GatedIo>,
    DuplexStream,
    Arc<WriteGate>,
    Vec<u8>,
) {
    // Allow the 14-byte masked Ping, then three bytes of the next data frame.
    let (io, peer, gate) = connection(17);
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (reader, writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(1001)).await;
    tokio::task::yield_now().await;
    let frame = gate.bytes.lock().unwrap().clone();
    assert_eq!(&frame[..2], &[0x89, 0x88]);
    let payload = frame[6..]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ frame[2 + i % 4])
        .collect();
    (reader, writer, peer, gate, payload)
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn data_does_not_postpone_pong_timeout_during_blocked_write() {
    let (mut reader, mut writer, mut peer, gate, _) = outstanding_ping().await;
    let send = tokio::spawn(async move {
        let result = writer.send_text("active").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    peer.write_all(b"\x82\x01x").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    tokio::time::advance(Duration::from_millis(501)).await;
    tokio::task::yield_now().await;
    assert!(
        send.is_finished(),
        "ordinary activity must not postpone Pong expiry"
    );
    let (_writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::HeartbeatTimeout)));
    assert_eq!(gate.bytes.lock().unwrap().len(), 17);
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn timely_pong_behind_peer_ping_keeps_blocked_write_alive() {
    let (mut reader, mut writer, mut peer, gate, payload) = outstanding_ping().await;
    let send = tokio::spawn(async move {
        let result = writer.send_text("active").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    let mut controls = b"\x89\x03old\x89\x03new\x8a\x08".to_vec();
    controls.extend_from_slice(&payload);
    peer.write_all(&controls).await.unwrap();
    // Queue control receipts without letting the driver observe them until expiry.
    for _ in 0..3 {
        assert!(matches!(
            poll!(std::pin::pin!(reader.next())),
            Poll::Ready(Some(Ok(_)))
        ));
    }
    tokio::time::advance(Duration::from_millis(501)).await;
    tokio::task::yield_now().await;
    assert!(!reader.is_closed());
    assert!(!send.is_finished());
    gate.release();
    let (_writer, result) = send.await.unwrap();
    result.unwrap();
    tokio::task::yield_now().await;
    let mut decoder = sockudo_ws::protocol::Protocol::new(sockudo_ws::Role::Server, 65536, 65536);
    let messages = decoder
        .process(&mut bytes::BytesMut::from(
            gate.bytes.lock().unwrap().as_slice(),
        ))
        .unwrap();
    assert!(matches!(&messages[..2], [Message::Ping(_), Message::Text(text)] if text == "active"));
    // RFC 6455 permits replies to earlier Pings as well as coalescing them.
    assert!(
        messages[2..]
            .iter()
            .all(|message| matches!(message, Message::Pong(_)))
    );
    assert!(matches!(messages.last(), Some(Message::Pong(pong)) if pong == b"new".as_slice()));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn deferred_ping_starts_pong_timeout_only_after_writing() {
    let (io, _peer, gate) = connection(3);
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let send = tokio::spawn(async move {
        let result = writer.send_text("active").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    assert!(!reader.is_closed());
    assert_eq!(gate.bytes.lock().unwrap().len(), 3);
    gate.release();
    let (_writer, result) = send.await.unwrap();
    result.unwrap();
    tokio::task::yield_now().await;
    let mut decoder = sockudo_ws::protocol::Protocol::new(sockudo_ws::Role::Server, 65536, 65536);
    let messages = decoder
        .process(&mut bytes::BytesMut::from(
            gate.bytes.lock().unwrap().as_slice(),
        ))
        .unwrap();
    assert!(matches!(&messages[..], [Message::Text(text), Message::Ping(_)] if text == "active"));
    tokio::time::advance(Duration::from_millis(900)).await;
    tokio::task::yield_now().await;
    assert!(!reader.is_closed());
    tokio::time::advance(Duration::from_millis(101)).await;
    assert!(matches!(
        reader.next().await,
        Some(Err(Error::HeartbeatTimeout))
    ));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn idle_timeout_interrupts_blocked_flush() {
    let (io, _peer, gate) = connection(usize::MAX);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = WebSocketStream::client(io, config).split();
    gate.flush_blocked.store(true, Ordering::Relaxed);
    let flush = tokio::spawn(async move {
        let result = writer.flush().await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(1001)).await;
    tokio::task::yield_now().await;
    assert!(flush.is_finished());
    let (_writer, result) = flush.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(gate.bytes.lock().unwrap().is_empty());
}

#[cfg(feature = "permessage-deflate")]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn compressed_write_observes_idle_timeout() {
    let (io, _peer, gate) = connection(3);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default(),
    )
    .split();
    let send = tokio::spawn(async move {
        let result = writer.send_text("compressed text".repeat(100)).await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(1001)).await;
    tokio::task::yield_now().await;
    assert!(send.is_finished());
    let (_writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(gate.dropped.load(Ordering::Relaxed));
    assert_eq!(gate.bytes.lock().unwrap().len(), 3);
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn pending_reader_observes_timeout_after_transport_release() {
    let (io, _peer, gate) = connection(3);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let read_gate = gate.clone();
    let read = tokio::spawn(async move {
        let result = reader.next().await;
        assert!(read_gate.dropped.load(Ordering::Relaxed));
        (reader, result)
    });
    let send = tokio::spawn(async move {
        let result = writer.send_text("active").await;
        (writer, result)
    });
    gate.blocked.notified().await;
    tokio::task::yield_now().await;
    assert!(!read.is_finished());
    tokio::time::advance(Duration::from_millis(1001)).await;
    let (_reader, result) = read.await.unwrap();
    assert!(matches!(result, Some(Err(Error::IdleTimeout))));
    let (_writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
}
