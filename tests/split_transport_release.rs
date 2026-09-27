#![cfg(feature = "tokio-runtime")]

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use sockudo_ws::{Config, Error, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::Notify;

struct WriteGate {
    remaining: AtomicUsize,
    bytes: Mutex<Vec<u8>>,
    blocked: Notify,
    shutdown_started: Notify,
    dropped: AtomicBool,
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
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
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

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.gate.shutdown_started.notify_one();
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
        shutdown_started: Notify::new(),
        dropped: AtomicBool::new(false),
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

fn decode_client_frame(bytes: &[u8]) -> (u8, Vec<u8>) {
    assert!(bytes.len() >= 6);
    let payload_len = usize::from(bytes[1] & 0x7f);
    assert_eq!(bytes.len(), 6 + payload_len);
    assert_ne!(bytes[1] & 0x80, 0);
    let mask = &bytes[2..6];
    let payload = bytes[6..]
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect();
    (bytes[0] & 0x0f, payload)
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_releases_transport_before_reporting_terminal_error() {
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
        let result = writer.send_text("incomplete frame").await;
        (writer, result)
    });

    gate.blocked.notified().await;
    tokio::time::advance(Duration::from_millis(1001)).await;

    let (_reader, result) = read.await.unwrap();
    assert!(matches!(result, Some(Err(Error::IdleTimeout))));
    let (mut writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(gate.bytes.lock().unwrap().len(), 3);
    assert!(matches!(
        writer.send_text("late").await,
        Err(Error::IdleTimeout)
    ));
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_closes_an_idle_sink_before_waking_a_queued_sender() {
    let (io, _peer, gate) = connection(usize::MAX);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(1)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let read_gate = gate.clone();
    let read = tokio::spawn(async move {
        let result = reader.next().await;
        assert!(read_gate.dropped.load(Ordering::Relaxed));
        result
    });

    tokio::time::advance(Duration::from_millis(1001)).await;
    gate.shutdown_started.notified().await;
    let send = tokio::spawn(async move { writer.send_text("late").await });
    tokio::task::yield_now().await;
    assert!(!send.is_finished());
    tokio::time::advance(Duration::from_millis(1001)).await;

    assert!(matches!(read.await.unwrap(), Some(Err(Error::IdleTimeout))));
    assert!(matches!(send.await.unwrap(), Err(Error::IdleTimeout)));
    assert!(gate.dropped.load(Ordering::Relaxed));
    let bytes = gate.bytes.lock().unwrap();
    let (opcode, payload) = decode_client_frame(&bytes);
    assert_eq!(opcode, 0x08);
    assert_eq!(&payload[..2], &1001u16.to_be_bytes());
    assert_eq!(&payload[2..], b"Connection idle timeout");
}

#[tokio::test(start_paused = true)]
async fn peer_eof_during_timeout_shutdown_does_not_replace_the_timeout_cause() {
    let (io, peer, gate) = connection(usize::MAX);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(1)
        .close_timeout(1)
        .build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();
    let read_gate = gate.clone();
    let read = tokio::spawn(async move {
        let result = reader.next().await;
        assert!(read_gate.dropped.load(Ordering::Relaxed));
        result
    });

    tokio::time::advance(Duration::from_millis(1001)).await;
    gate.shutdown_started.notified().await;
    drop(peer);
    tokio::task::yield_now().await;
    assert!(!read.is_finished());

    tokio::time::advance(Duration::from_millis(1001)).await;
    assert!(matches!(read.await.unwrap(), Some(Err(Error::IdleTimeout))));
    assert!(gate.dropped.load(Ordering::Relaxed));
}

#[tokio::test(start_paused = true)]
async fn local_close_deadline_releases_transport_with_live_handles() {
    let (io, _peer, gate) = connection(usize::MAX);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();

    writer.close(1000, "done").await.unwrap();
    tokio::time::advance(Duration::from_millis(1001)).await;
    gate.shutdown_started.notified().await;

    assert!(reader.next().await.is_none());
    assert!(gate.dropped.load(Ordering::Relaxed));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test(start_paused = true)]
async fn compressed_idle_timeout_releases_transport_with_live_handles() {
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

    let (_writer, result) = send.await.unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(gate.dropped.load(Ordering::Relaxed));
    assert_eq!(gate.bytes.lock().unwrap().len(), 3);
}
