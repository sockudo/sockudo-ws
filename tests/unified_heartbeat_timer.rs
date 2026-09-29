#![cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]

use std::cell::Cell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use futures_util::future::Either;
use futures_util::{SinkExt, Stream};
use sockudo_ws::{CompressedWebSocketStream, Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

struct BlockedControlIo {
    read: DuplexStream,
    block_flush: bool,
    blocked: Rc<Cell<bool>>,
}

impl AsyncRead for BlockedControlIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl AsyncWrite for BlockedControlIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.block_flush || !self.blocked.get() {
            Poll::Ready(Ok(bytes.len()))
        } else {
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.blocked.get() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[rstest::rstest]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn idle_deadline_wakes_and_terminates_a_blocked_control_write(
    #[values(false, true)] compressed: bool,
    #[values(false, true)] outgoing_ping: bool,
    #[values(false, true)] block_flush: bool,
) {
    let (read, mut peer) = tokio::io::duplex(128);
    let io = BlockedControlIo {
        read,
        block_flush,
        blocked: Rc::new(Cell::new(true)),
    };
    let config = Config::builder()
        .auto_ping(outgoing_ping)
        .ping_interval(1)
        .idle_timeout(2)
        .close_timeout(0)
        .build();
    let mut ws: Pin<Box<dyn Stream<Item = sockudo_ws::Result<Message>>>> = if compressed {
        Box::pin(CompressedWebSocketStream::client(
            io,
            config,
            Default::default(),
        ))
    } else {
        Box::pin(WebSocketStream::client(io, config))
    };
    if !outgoing_ping {
        peer.write_all(b"\x89\x00").await.unwrap();
    }
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(ws.as_mut().poll_next(&mut cx).is_pending());
    if outgoing_ping {
        tokio::time::advance(Duration::from_millis(1001)).await;
        assert!(ws.as_mut().poll_next(&mut cx).is_pending());
    }
    wakes.0.store(0, Ordering::Relaxed);

    tokio::time::advance(Duration::from_millis(if outgoing_ping {
        1000
    } else {
        2001
    }))
    .await;
    tokio::task::yield_now().await;

    assert!(wakes.0.load(Ordering::Relaxed) > 0);
    assert!(matches!(
        ws.as_mut().poll_next(&mut cx),
        Poll::Ready(Some(Err(Error::IdleTimeout)))
    ));
}

#[rstest::rstest]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn queued_data_write_keeps_idle_deadline_active(
    #[values(false, true)] compressed: bool,
    #[values(false, true)] block_flush: bool,
) {
    let (read, _peer) = tokio::io::duplex(128);
    let io = BlockedControlIo {
        read,
        block_flush,
        blocked: Rc::new(Cell::new(true)),
    };
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(1)
        .close_timeout(0)
        .write_coalescing(true)
        .build();
    let mut ws = if compressed {
        Either::Left(CompressedWebSocketStream::client(
            io,
            config,
            Default::default(),
        ))
    } else {
        Either::Right(WebSocketStream::client(io, config))
    };
    ws.feed(Message::text("pending data")).await.unwrap();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut ws).poll_next(&mut cx).is_pending());
    wakes.0.store(0, Ordering::Relaxed);

    tokio::time::advance(Duration::from_millis(1001)).await;
    tokio::task::yield_now().await;

    assert!(wakes.0.load(Ordering::Relaxed) > 0);
    assert!(matches!(
        Pin::new(&mut ws).poll_next(&mut cx),
        Poll::Ready(Some(Err(Error::IdleTimeout)))
    ));
}

#[rstest::rstest]
#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn outstanding_ping_timeout_survives_a_blocked_pong_response(
    #[values(false, true)] compressed: bool,
    #[values(false, true)] block_flush: bool,
) {
    let (read, mut peer) = tokio::io::duplex(128);
    let blocked = Rc::new(Cell::new(false));
    let io = BlockedControlIo {
        read,
        block_flush,
        blocked: blocked.clone(),
    };
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let mut ws = if compressed {
        Either::Left(CompressedWebSocketStream::client(
            io,
            config,
            Default::default(),
        ))
    } else {
        Either::Right(WebSocketStream::client(io, config))
    };
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut ws).poll_next(&mut cx).is_pending());
    tokio::time::advance(Duration::from_millis(1001)).await;
    assert!(Pin::new(&mut ws).poll_next(&mut cx).is_pending());
    blocked.set(true);
    peer.write_all(b"\x89\x00").await.unwrap();
    assert!(Pin::new(&mut ws).poll_next(&mut cx).is_pending());
    wakes.0.store(0, Ordering::Relaxed);

    tokio::time::advance(Duration::from_millis(1001)).await;
    tokio::task::yield_now().await;

    assert!(wakes.0.load(Ordering::Relaxed) > 0);
    assert!(matches!(
        Pin::new(&mut ws).poll_next(&mut cx),
        Poll::Ready(Some(Err(Error::HeartbeatTimeout)))
    ));
}
