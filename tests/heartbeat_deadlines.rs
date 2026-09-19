#![cfg(feature = "tokio-runtime")]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::BytesMut;
use futures_util::Stream;
use futures_util::task::{ArcWake, waker_ref};
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::{Config, Error, Message, Result, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const PAYLOAD: &[u8] = &[b'x'; 64];

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn inbound_activity_delays_idle_timeout_without_losing_timer_wakeup() {
    check_idle_deadline(WebSocketStream::server, data_frame()).await;
}

#[cfg(feature = "permessage-deflate")]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn compressed_activity_delays_idle_timeout_without_losing_timer_wakeup() {
    check_idle_deadline(compressed_server, compressed_data_frame()).await;
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn matching_pong_moves_the_next_ping_before_the_old_pong_deadline() {
    check_earlier_ping_deadline(WebSocketStream::server).await;
}

#[cfg(feature = "permessage-deflate")]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn compressed_matching_pong_moves_ping_before_the_old_pong_deadline() {
    check_earlier_ping_deadline(compressed_server).await;
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn ordinary_activity_preserves_the_outstanding_pong_deadline() {
    check_pong_deadline(WebSocketStream::server, data_frame()).await;
}

#[cfg(feature = "permessage-deflate")]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn compressed_activity_preserves_the_outstanding_pong_deadline() {
    check_pong_deadline(compressed_server, compressed_data_frame()).await;
}

async fn check_idle_deadline<S>(
    server: impl FnOnce(DuplexStream, Config) -> S,
    data_frame: BytesMut,
) where
    S: Stream<Item = Result<Message>> + Unpin,
{
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let mut ws = server(io, config);
    let wake = Arc::new(TimerWake::default());
    assert!(poll_next(&mut ws, &wake).is_pending());

    tokio::time::advance(Duration::from_millis(500)).await;
    peer.write_all(&data_frame).await.unwrap();
    assert!(matches!(
        poll_next(&mut ws, &wake),
        Poll::Ready(Some(Ok(message))) if message.as_bytes() == PAYLOAD
    ));
    assert!(poll_next(&mut ws, &wake).is_pending());

    // The old idle registration may fire, but newer data postpones expiry.
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(poll_next(&mut ws, &wake).is_pending());
    advance_until_woken(Duration::from_millis(401), &wake).await;
    assert!(matches!(
        poll_next(&mut ws, &wake),
        Poll::Ready(Some(Err(Error::IdleTimeout)))
    ));
}

async fn check_earlier_ping_deadline<S>(server: impl FnOnce(DuplexStream, Config) -> S)
where
    S: Stream<Item = Result<Message>> + Unpin,
{
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder()
        .idle_timeout(0)
        .ping_interval(1)
        .pong_timeout(30)
        .build();
    let mut ws = server(io, config);
    let wake = Arc::new(TimerWake::default());
    assert!(poll_next(&mut ws, &wake).is_pending());
    advance_until_woken(Duration::from_millis(1001), &wake).await;
    assert!(poll_next(&mut ws, &wake).is_pending());
    let first_ping = read_ping(&mut peer).await;

    let mut pong = BytesMut::new();
    encode_frame(
        &mut pong,
        OpCode::Pong,
        &first_ping,
        true,
        Some([1, 2, 3, 4]),
    );
    peer.write_all(&pong).await.unwrap();
    assert!(matches!(
        poll_next(&mut ws, &wake),
        Poll::Ready(Some(Ok(Message::Pong(payload)))) if payload == first_ping[..]
    ));
    assert!(poll_next(&mut ws, &wake).is_pending());

    advance_until_woken(Duration::from_millis(1001), &wake).await;
    assert!(poll_next(&mut ws, &wake).is_pending());
    assert_ne!(read_ping(&mut peer).await, first_ping);
}

async fn check_pong_deadline<S>(
    server: impl FnOnce(DuplexStream, Config) -> S,
    data_frame: BytesMut,
) where
    S: Stream<Item = Result<Message>> + Unpin,
{
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder()
        .idle_timeout(0)
        .ping_interval(1)
        .pong_timeout(1)
        .build();
    let mut ws = server(io, config);
    let wake = Arc::new(TimerWake::default());
    assert!(poll_next(&mut ws, &wake).is_pending());
    advance_until_woken(Duration::from_millis(1001), &wake).await;
    assert!(poll_next(&mut ws, &wake).is_pending());
    let _ = read_ping(&mut peer).await;

    tokio::time::advance(Duration::from_millis(500)).await;
    peer.write_all(&data_frame).await.unwrap();
    assert!(matches!(
        poll_next(&mut ws, &wake),
        Poll::Ready(Some(Ok(message))) if message.as_bytes() == PAYLOAD
    ));
    assert!(poll_next(&mut ws, &wake).is_pending());

    advance_until_woken(Duration::from_millis(501), &wake).await;
    assert!(matches!(
        poll_next(&mut ws, &wake),
        Poll::Ready(Some(Err(Error::HeartbeatTimeout)))
    ));
}

#[derive(Default)]
struct TimerWake(AtomicBool);

impl ArcWake for TimerWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.store(true, Ordering::Relaxed);
    }
}

fn poll_next<S>(ws: &mut S, wake: &Arc<TimerWake>) -> Poll<Option<Result<Message>>>
where
    S: Stream<Item = Result<Message>> + Unpin,
{
    let waker = waker_ref(wake);
    Pin::new(ws).poll_next(&mut Context::from_waker(&waker))
}

async fn advance_until_woken(duration: Duration, wake: &TimerWake) {
    wake.0.store(false, Ordering::Relaxed);
    tokio::time::advance(duration).await;
    tokio::task::yield_now().await;
    // Check before polling the stream: an unsolicited repoll could hide a lost wakeup.
    assert!(
        wake.0.load(Ordering::Relaxed),
        "timer did not wake the reader"
    );
}

async fn read_ping(peer: &mut DuplexStream) -> [u8; 8] {
    let mut frame = [0; 10];
    peer.read_exact(&mut frame).await.unwrap();
    assert_eq!(&frame[..2], &[0x89, 8]);
    frame[2..].try_into().unwrap()
}

fn data_frame() -> BytesMut {
    let mut frame = BytesMut::new();
    encode_frame(
        &mut frame,
        OpCode::Binary,
        PAYLOAD,
        true,
        Some([1, 2, 3, 4]),
    );
    frame
}

#[cfg(feature = "permessage-deflate")]
fn compressed_server(
    io: DuplexStream,
    config: Config,
) -> sockudo_ws::CompressedWebSocketStream<DuplexStream> {
    sockudo_ws::CompressedWebSocketStream::server(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default(),
    )
}

#[cfg(feature = "permessage-deflate")]
fn compressed_data_frame() -> BytesMut {
    let mut protocol = sockudo_ws::protocol::CompressedProtocol::client(
        1024,
        1024,
        sockudo_ws::deflate::DeflateConfig::default(),
    );
    let mut frame = BytesMut::new();
    protocol
        .encode_message(&Message::binary(PAYLOAD), &mut frame)
        .unwrap();
    assert_ne!(frame[0] & 0x40, 0, "fixture must contain compressed data");
    frame
}
