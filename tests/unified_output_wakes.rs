#![cfg(feature = "tokio-runtime")]

use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream};
use sockudo_ws::{Config, Error, Message};

#[path = "support/wake_count.rs"]
mod wake_count;

use wake_count::WakeCount;

#[derive(Default)]
struct IoState {
    readable: Vec<u8>,
    allowance: usize,
    flush_ready: bool,
    write_error: bool,
    zero_write: bool,
    // Some transports keep one waker for both directions.
    read_overwrites_write_waker: bool,
    written: Vec<u8>,
    write_waker: Option<Waker>,
}

struct MockIo(Rc<RefCell<IoState>>);

impl tokio::io::AsyncRead for MockIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.borrow_mut();
        if state.readable.is_empty() {
            if state.read_overwrites_write_waker {
                state.write_waker = Some(cx.waker().clone());
            }
            return Poll::Pending;
        }
        let count = state.readable.len().min(buf.remaining());
        buf.put_slice(&state.readable[..count]);
        state.readable.drain(..count);
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for MockIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.write_error {
            return Poll::Ready(Err(io::Error::other("write failed")));
        }
        if state.zero_write {
            return Poll::Ready(Ok(0));
        }
        if state.allowance == 0 {
            state.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let count = state.allowance.min(buf.len());
        state.written.extend_from_slice(&buf[..count]);
        state.allowance -= count;
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.borrow_mut();
        if state.flush_ready {
            Poll::Ready(Ok(()))
        } else {
            state.write_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct Task {
    wakes: Arc<WakeCount>,
    waker: Waker,
}

impl Task {
    fn new() -> Self {
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        Self { wakes, waker }
    }

    fn cx(&self) -> Context<'_> {
        Context::from_waker(&self.waker)
    }

    fn take_wakes(&self) -> usize {
        self.wakes.0.swap(0, Ordering::Relaxed)
    }
}

fn io_state(allowance: usize, flush_ready: bool) -> Rc<RefCell<IoState>> {
    Rc::new(RefCell::new(IoState {
        allowance,
        flush_ready,
        ..Default::default()
    }))
}

fn quiet_config() -> Config {
    Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(30)
        .build()
}

// A masked client Ping with an empty payload.
const PEER_PING: [u8; 6] = [0x89, 0x80, 0, 0, 0, 0];

macro_rules! output_wake_cases {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;

            fn poll_next<S: Stream + Unpin>(ws: &mut S, task: &Task) -> Poll<Option<S::Item>> {
                Pin::new(ws).poll_next(&mut task.cx())
            }

            fn poll_flush<S: Sink<Message> + Unpin>(
                ws: &mut S,
                task: &Task,
            ) -> Poll<Result<(), S::Error>> {
                Pin::new(ws).poll_flush(&mut task.cx())
            }

            fn poll_close<S: Sink<Message> + Unpin>(
                ws: &mut S,
                task: &Task,
            ) -> Poll<Result<(), S::Error>> {
                Pin::new(ws).poll_close(&mut task.cx())
            }

            #[tokio::test]
            async fn read_side_write_error_wakes_parked_sink() {
                let state = io_state(0, true);
                let mut ws = ($make)(MockIo(state.clone()), quiet_config());
                ws.feed(Message::text("queued")).await.unwrap();
                let (reader, writer) = (Task::new(), Task::new());
                assert!(poll_flush(&mut ws, &writer).is_pending());

                state.borrow_mut().write_error = true;
                assert!(matches!(
                    poll_next(&mut ws, &reader),
                    Poll::Ready(Some(Err(_)))
                ));
                assert!(writer.take_wakes() > 0);
                assert!(matches!(poll_flush(&mut ws, &writer), Poll::Ready(Err(_))));
            }

            #[tokio::test]
            async fn sink_close_wakes_reader_parked_on_read() {
                let state = io_state(usize::MAX, true);
                let mut ws = ($make)(MockIo(state), quiet_config());
                let (reader, writer) = (Task::new(), Task::new());
                assert!(poll_next(&mut ws, &reader).is_pending());

                assert!(matches!(poll_close(&mut ws, &writer), Poll::Ready(Ok(()))));
                assert!(reader.take_wakes() > 0);
                assert!(matches!(poll_next(&mut ws, &reader), Poll::Ready(None)));
            }

            // The transport keeps one write waker. When the read registers
            // last, its waker must still reach the parked sink.
            #[tokio::test]
            async fn read_registered_transport_waker_reaches_sink() {
                let state = io_state(0, true);
                state.borrow_mut().read_overwrites_write_waker = true;
                let mut ws = ($make)(MockIo(state.clone()), quiet_config());
                ws.feed(Message::text("queued")).await.unwrap();
                let (reader, writer) = (Task::new(), Task::new());
                assert!(poll_flush(&mut ws, &writer).is_pending());
                assert!(poll_next(&mut ws, &reader).is_pending());

                state.borrow_mut().allowance = usize::MAX;
                let waker = state.borrow_mut().write_waker.take().unwrap();
                waker.wake();
                assert!(writer.take_wakes() > 0);
                assert!(matches!(poll_flush(&mut ws, &writer), Poll::Ready(Ok(()))));
            }

            // Each wake consumes the registration; the next Pending poll must
            // register again even though its waker is unchanged.
            #[tokio::test]
            async fn repeated_pending_flush_receives_each_wake() {
                let state = io_state(0, true);
                let mut ws = ($make)(MockIo(state.clone()), quiet_config());
                ws.feed(Message::text("queued")).await.unwrap();
                let writer = Task::new();
                for _ in 0..2 {
                    assert!(poll_flush(&mut ws, &writer).is_pending());
                    let waker = state.borrow_mut().write_waker.take().unwrap();
                    waker.wake();
                    assert!(writer.take_wakes() > 0);
                }
            }

            #[tokio::test]
            async fn sink_moved_to_another_task_receives_the_wake() {
                let state = io_state(0, true);
                let mut ws = ($make)(MockIo(state.clone()), quiet_config());
                ws.feed(Message::text("queued")).await.unwrap();
                let (first, second) = (Task::new(), Task::new());
                assert!(poll_flush(&mut ws, &first).is_pending());
                assert!(poll_flush(&mut ws, &second).is_pending());

                let waker = state.borrow_mut().write_waker.take().unwrap();
                waker.wake();
                assert!(second.take_wakes() > 0);
            }

            // Halves on different worker threads exercise cross-thread wakes
            // racing with registration; a lost wake shows up as a timeout.
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn split_halves_progress_under_cross_thread_wakes() {
                use futures_util::StreamExt;
                use tokio::io::{AsyncReadExt, AsyncWriteExt};

                const MESSAGES: usize = 200;
                for _ in 0..100 {
                    let (io, peer) = tokio::io::duplex(64);
                    let ws = ($make)(io, quiet_config());
                    let (mut sink, mut stream) = StreamExt::split(ws);
                    let (mut peer_read, mut peer_write) = tokio::io::split(peer);
                    let writer = tokio::spawn(async move {
                        for index in 0..MESSAGES {
                            sink.send(Message::binary(vec![index as u8; 100]))
                                .await
                                .unwrap();
                        }
                        sink
                    });
                    let pinger = tokio::spawn(async move {
                        for _ in 0..MESSAGES {
                            peer_write.write_all(&PEER_PING).await.unwrap();
                            tokio::task::yield_now().await;
                        }
                        peer_write
                    });
                    let reader = tokio::spawn(async move {
                        for _ in 0..MESSAGES {
                            assert!(matches!(stream.next().await, Some(Ok(Message::Ping(_)))));
                        }
                        stream
                    });
                    let drain = tokio::spawn(async move {
                        let mut buf = [0; 37];
                        while peer_read.read(&mut buf).await.unwrap() != 0 {}
                    });
                    let finished = tokio::time::timeout(Duration::from_secs(10), async {
                        (
                            writer.await.unwrap(),
                            pinger.await.unwrap(),
                            reader.await.unwrap(),
                        )
                    })
                    .await
                    .expect("a half stalled after a cross-thread wake");
                    drain.abort();
                    drop(finished);
                }
            }

            #[tokio::test]
            async fn empty_write_progress_is_connection_closed() {
                let state = io_state(usize::MAX, true);
                state.borrow_mut().zero_write = true;
                let mut ws = ($make)(MockIo(state), quiet_config());
                assert!(matches!(
                    ws.send(Message::text("lost")).await,
                    Err(Error::ConnectionClosed)
                ));
            }

            // Each half registers on the shared closing timer; whichever
            // registered last may stop polling without stranding the other.
            #[rstest::rstest]
            #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
            #[tokio::test(start_paused = true)]
            async fn closing_timer_wakes_both_halves(#[values(false, true)] reader_first: bool) {
                let state = io_state(0, true);
                let config = Config::builder()
                    .auto_ping(false)
                    .idle_timeout(0)
                    .close_timeout(1)
                    .build();
                let mut ws = ($make)(MockIo(state), config);
                for message in [Message::binary(vec![7; 64]), Message::Close(None)] {
                    Pin::new(&mut ws).start_send(message).unwrap();
                }
                let (reader, writer) = (Task::new(), Task::new());
                if reader_first {
                    assert!(poll_next(&mut ws, &reader).is_pending());
                    assert!(poll_close(&mut ws, &writer).is_pending());
                } else {
                    assert!(poll_close(&mut ws, &writer).is_pending());
                    assert!(poll_next(&mut ws, &reader).is_pending());
                }
                reader.take_wakes();
                writer.take_wakes();

                tokio::time::advance(Duration::from_millis(1001)).await;
                tokio::task::yield_now().await;

                assert!(reader.take_wakes() > 0);
                assert!(writer.take_wakes() > 0);
                assert!(matches!(
                    poll_next(&mut ws, &reader),
                    Poll::Ready(Some(Err(Error::ConnectionClosed)))
                ));
            }

            // A Ping flushed by the sink starts the Pong timeout then, not
            // when the reader next polls.
            #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
            #[tokio::test(start_paused = true)]
            async fn sink_ping_flush_starts_pong_timeout() {
                let state = io_state(0, true);
                let config = Config::builder()
                    .ping_interval(1)
                    .pong_timeout(2)
                    .idle_timeout(0)
                    .close_timeout(0)
                    .build();
                let mut ws = ($make)(MockIo(state.clone()), config);
                let (reader, writer) = (Task::new(), Task::new());
                assert!(poll_next(&mut ws, &reader).is_pending());
                tokio::time::advance(Duration::from_millis(1001)).await;
                assert!(poll_next(&mut ws, &reader).is_pending());
                reader.take_wakes();

                state.borrow_mut().allowance = usize::MAX;
                assert!(matches!(poll_flush(&mut ws, &writer), Poll::Ready(Ok(()))));
                assert!(reader.take_wakes() > 0);

                tokio::time::advance(Duration::from_secs(9)).await;
                assert!(matches!(
                    poll_next(&mut ws, &reader),
                    Poll::Ready(Some(Err(Error::HeartbeatTimeout)))
                ));
            }

            // Flushing the Ping starts its timeout even if the Pong queued
            // behind it then blocks.
            #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
            #[tokio::test(start_paused = true)]
            async fn ping_flush_counts_before_blocked_pong() {
                let state = io_state(0, true);
                let config = Config::builder()
                    .ping_interval(1)
                    .pong_timeout(1)
                    .idle_timeout(0)
                    .close_timeout(0)
                    .build();
                let mut ws = ($make)(MockIo(state.clone()), config);
                let reader = Task::new();
                assert!(poll_next(&mut ws, &reader).is_pending());
                tokio::time::advance(Duration::from_millis(1001)).await;
                assert!(poll_next(&mut ws, &reader).is_pending());

                state.borrow_mut().readable.extend_from_slice(&PEER_PING);
                assert!(matches!(
                    poll_next(&mut ws, &reader),
                    Poll::Ready(Some(Ok(Message::Ping(_))))
                ));
                // Exactly the 10-byte server Ping: the Pong behind it blocks.
                state.borrow_mut().allowance = 10;
                assert!(poll_next(&mut ws, &reader).is_pending());
                assert_eq!(state.borrow().written.len(), 10);
                reader.take_wakes();

                tokio::time::advance(Duration::from_millis(1001)).await;
                tokio::task::yield_now().await;
                assert!(reader.take_wakes() > 0);
                assert!(matches!(
                    poll_next(&mut ws, &reader),
                    Poll::Ready(Some(Err(Error::HeartbeatTimeout)))
                ));
            }
        }
    };
}

output_wake_cases!(plain, sockudo_ws::WebSocketStream::server);
#[cfg(feature = "permessage-deflate")]
output_wake_cases!(compressed, |io, config| {
    sockudo_ws::CompressedWebSocketStream::server(
        io,
        config,
        sockudo_ws::DeflateConfig {
            compression_threshold: usize::MAX,
            ..Default::default()
        },
    )
});
