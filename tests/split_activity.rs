#![cfg(feature = "tokio-runtime")]

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{poll, task::AtomicWaker};
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::Notify;

#[derive(Default)]
struct WriteGate {
    entered: Notify,
    released: AtomicBool,
    waker: AtomicWaker,
}

struct GatedIo {
    inner: DuplexStream,
    gate: Arc<WriteGate>,
}

impl AsyncRead for GatedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for GatedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.gate.waker.register(cx.waker());
        if self.gate.released.load(Ordering::Relaxed) {
            Poll::Ready(Ok(bytes.len()))
        } else {
            self.gate.entered.notify_one();
            Poll::Pending
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn blocked_connection() -> (GatedIo, Arc<WriteGate>) {
    let (inner, mut peer) = tokio::io::duplex(4096);
    for sequence in 0..64u8 {
        peer.write_all(&[0x82, 1, sequence]).await.unwrap();
    }
    // All input is buffered; EOF follows only after the 64 messages.
    let gate = Arc::new(WriteGate::default());
    (
        GatedIo {
            inner,
            gate: gate.clone(),
        },
        gate,
    )
}

#[tokio::test]
async fn ordinary_messages_pass_a_blocked_writer() {
    let (io, gate) = blocked_connection().await;
    let (mut reader, mut writer) = WebSocketStream::client(io, Config::default()).split();
    let send = tokio::spawn(async move { writer.send(Message::binary(b"out".as_slice())).await });
    gate.entered.notified().await;
    for sequence in 0..64u8 {
        let message = tokio::time::timeout(Duration::from_secs(1), reader.next())
            .await
            .expect("ordinary reads must not await the blocked writer")
            .unwrap()
            .unwrap();
        assert_eq!(message.as_bytes(), &[sequence]);
    }
    drop(reader);
    assert!(send.await.unwrap().is_err());
}

#[tokio::test]
async fn cancelled_activity_enqueue_does_not_skip_a_message() {
    let (io, gate) = blocked_connection().await;
    let (mut reader, mut writer) = WebSocketStream::client(io, Config::default()).split();
    let send = tokio::spawn(async move {
        let result = writer.send(Message::binary(b"out".as_slice())).await;
        (writer, result)
    });
    gate.entered.notified().await;
    for sequence in 0..32u8 {
        assert_eq!(
            reader.next().await.unwrap().unwrap().as_bytes(),
            &[sequence]
        );
    }
    let observed = {
        let mut next = std::pin::pin!(reader.next());
        match poll!(next.as_mut()) {
            Poll::Ready(message) => message,
            Poll::Pending => None,
        }
    };
    gate.released.store(true, Ordering::Relaxed);
    gate.waker.wake();
    let (_writer, result) = send.await.unwrap();
    result.unwrap();
    let message = match observed {
        Some(message) => message,
        None => reader.next().await.unwrap(),
    }
    .unwrap();
    assert_eq!(message.as_bytes(), &[32]);
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_messages_pass_a_blocked_writer() {
    let (io, gate) = blocked_connection().await;
    let ws = sockudo_ws::CompressedWebSocketStream::client(
        io,
        Config::default(),
        sockudo_ws::deflate::DeflateConfig::default(),
    );
    let (mut reader, mut writer) = ws.split();
    let send = tokio::spawn(async move { writer.send(Message::binary(b"out".as_slice())).await });
    gate.entered.notified().await;
    for sequence in 0..64u8 {
        let message = tokio::time::timeout(Duration::from_secs(1), reader.next())
            .await
            .expect("ordinary reads must not await the blocked writer")
            .unwrap()
            .unwrap();
        assert_eq!(message.as_bytes(), &[sequence]);
    }
    drop(reader);
    assert!(send.await.unwrap().is_err());
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn data_activity_defers_split_idle_timeout() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    peer.write_all(b"\x81\x01x").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;
    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
    tokio::time::advance(Duration::from_millis(401)).await;
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
}

async fn outstanding_ping() -> (
    sockudo_ws::SplitReader<DuplexStream>,
    sockudo_ws::SplitWriter<DuplexStream>,
    DuplexStream,
    Vec<u8>,
) {
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (reader, writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(1000)).await;
    let mut frame = [0; 14];
    peer.read_exact(&mut frame).await.unwrap();
    assert_eq!(&frame[..2], &[0x89, 0x88]);
    let payload = frame[6..]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ frame[2 + i % 4])
        .collect();
    (reader, writer, peer, payload)
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn ordinary_data_does_not_postpone_pong_timeout() {
    let (mut reader, _writer, mut peer, _) = outstanding_ping().await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(300)).await;
        peer.write_all(b"\x82\x01x").await.unwrap();
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    }
    tokio::time::advance(Duration::from_millis(101)).await;
    assert!(matches!(
        reader.next().await,
        Some(Err(Error::HeartbeatTimeout))
    ));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn queued_timely_pong_precedes_ready_timeout() {
    let (mut reader, mut writer, mut peer, payload) = outstanding_ping().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    let mut frame = vec![0x8a, 8];
    frame.extend_from_slice(&payload);
    peer.write_all(&frame).await.unwrap();
    // Queue the Pong without yielding to the writer, then make its timer due.
    assert!(matches!(
        poll!(std::pin::pin!(reader.next())),
        Poll::Ready(Some(Ok(Message::Pong(_))))
    ));
    // Keep the accepted request alive: cancelling a send closes the connection.
    let mut queued = std::pin::pin!(writer.send_text("queued"));
    assert!(poll!(queued.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(501)).await;
    tokio::task::yield_now().await;
    let mut opcode = [0];
    peer.read_exact(&mut opcode).await.unwrap();
    assert_eq!(
        opcode[0], 0x81,
        "timely Pong must allow queued data to proceed"
    );
    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn ordinary_activity_postpones_ping_until_reads_stop() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder().ping_interval(1).idle_timeout(0).build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    for _ in 0..4 {
        tokio::time::advance(Duration::from_millis(500)).await;
        peer.write_all(b"\x82\x01x").await.unwrap();
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
        tokio::task::yield_now().await;
        let mut byte = [0];
        assert!(poll!(std::pin::pin!(peer.read(&mut byte))).is_pending());
    }
    tokio::time::advance(Duration::from_millis(1001)).await;
    let mut byte = [0];
    peer.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, [0x89]);
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn expired_idle_deadline_precedes_queued_application_writes() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let ws = WebSocketStream::server(io, config);
    let (mut reader, mut writer) = ws.split();
    tokio::task::yield_now().await;
    // Keep the accepted request alive until its deadline is resolved.
    let mut send = std::pin::pin!(writer.send(Message::binary(b"x".as_slice())));
    assert!(poll!(send.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(1001)).await;
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
    let mut first_byte = [0];
    peer.read_exact(&mut first_byte).await.unwrap();
    assert_eq!(
        first_byte,
        [0x88],
        "close must precede queued data after timeout"
    );
}

#[tokio::test]
async fn eof_after_buffered_data_terminates_split_reads() {
    let (io, mut peer) = tokio::io::duplex(64);
    peer.write_all(b"\x82\x01x").await.unwrap();
    peer.shutdown().await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    assert!(reader.next().await.is_none());
    assert!(reader.next().await.is_none());
    assert!(matches!(
        writer.send_text("closed").await,
        Err(Error::ConnectionClosed)
    ));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn expired_idle_timeout_precedes_queued_application_writes() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    // Keep the accepted request alive: cancelling a send closes the connection.
    let mut queued = std::pin::pin!(writer.send_text("queued"));
    assert!(poll!(queued.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(1001)).await;
    let mut opcode = [0];
    peer.read_exact(&mut opcode).await.unwrap();
    assert_eq!(
        opcode[0], 0x88,
        "Close must precede queued data after expiry"
    );
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn due_ping_precedes_queued_application_writes() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder().ping_interval(1).idle_timeout(0).build();
    let (_reader, mut writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    // Keep the accepted request alive: cancelling a send closes the connection.
    let mut queued = std::pin::pin!(writer.send_text("queued"));
    assert!(poll!(queued.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(1001)).await;
    let mut opcode = [0];
    peer.read_exact(&mut opcode).await.unwrap();
    assert_eq!(opcode[0], 0x89, "Ping must precede queued data when due");
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn expired_pong_timeout_precedes_queued_application_writes() {
    let (mut reader, mut writer, mut peer, _) = outstanding_ping().await;
    // Keep the accepted request alive: cancelling a send closes the connection.
    let mut queued = std::pin::pin!(writer.send_text("queued"));
    assert!(poll!(queued.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(1001)).await;
    let mut opcode = [0];
    peer.read_exact(&mut opcode).await.unwrap();
    assert_eq!(opcode[0], 0x88, "Pong timeout must precede queued data");
    assert!(matches!(
        reader.next().await,
        Some(Err(Error::HeartbeatTimeout))
    ));
}
