#![cfg(feature = "tokio-runtime")]

use std::cell::Cell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

struct ShutdownIo {
    input: DuplexStream,
    pending_shutdown: bool,
    fail_flush: bool,
    block_write: bool,
    block_after_local_close: bool,
    writes: Rc<Cell<usize>>,
    shutdowns: Rc<Cell<usize>>,
}

impl AsyncRead for ShutdownIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.input).poll_read(cx, buf)
    }
}

impl AsyncWrite for ShutdownIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.writes.set(self.writes.get() + 1);
        if self.block_after_local_close && self.writes.get() > 1 {
            return Poll::Pending;
        }
        if self.block_write {
            return if self.writes.get() == 1 {
                Poll::Ready(Ok(bytes.len().min(3)))
            } else {
                Poll::Pending
            };
        }
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail_flush {
            Poll::Ready(Err(io::Error::other("flush failed")))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdowns.set(self.shutdowns.get() + 1);
        if self.pending_shutdown {
            Poll::Pending
        } else {
            Poll::Ready(Err(io::Error::other("shutdown failed")))
        }
    }
}

fn connection(pending_shutdown: bool, fail_flush: bool) -> (ShutdownIo, DuplexStream) {
    let (input, peer) = tokio::io::duplex(128);
    (
        ShutdownIo {
            input,
            pending_shutdown,
            fail_flush,
            block_write: false,
            block_after_local_close: false,
            writes: Rc::new(Cell::new(0)),
            shutdowns: Rc::new(Cell::new(0)),
        },
        peer,
    )
}

fn config() -> Config {
    Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build()
}

macro_rules! close_cases {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;

            #[tokio::test(start_paused = true)]
            async fn post_expiry_read_is_not_renewed_after_pre_expiry_input() {
                let (io, mut peer) = connection(false, false);
                let mut ws = ($make)(io, config());
                ws.close(1000, "").await.unwrap();
                peer.write_all(b"\x89\x01p").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                tokio::time::advance(Duration::from_secs(2)).await;
                peer.write_all(b"\x89\x01p").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                peer.write_all(b"\x89\x01p").await.unwrap();
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn pong_write_timeout_terminates_without_draining_data() {
                let (mut io, mut peer) = connection(false, false);
                io.block_after_local_close = true;
                let mut ws = ($make)(io, config());
                ws.close(1000, "").await.unwrap();
                peer.write_all(b"\x89\x01p\x82\x01d").await.unwrap();
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn accepted_close_before_parse_error_suppresses_all_pongs() {
                let (io, mut peer) = connection(false, false);
                let writes = io.writes.clone();
                let mut ws = ($make)(io, config());
                peer.write_all(b"\x89\x01p\x89\x01q\x88\x02\x03\xe8\x83\x00")
                    .await
                    .unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert_eq!(writes.get(), 0);
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn expired_nonzero_budget_reads_ready_close_once() {
                let (io, mut peer) = connection(true, false);
                let mut ws = ($make)(io, config());
                ws.close(1000, "").await.unwrap();
                peer.write_all(b"\x88\x02\x03\xe8").await.unwrap();
                tokio::time::advance(Duration::from_secs(2)).await;
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn zero_budget_pending_read_terminates_on_first_poll() {
                let (io, _peer) = connection(false, false);
                let mut cfg = config();
                cfg.close_timeout = 0;
                let mut ws = ($make)(io, cfg);
                ws.close(1000, "").await.unwrap();
                assert!(matches!(
                    futures_util::poll!(ws.next()),
                    Poll::Ready(Some(Err(Error::ConnectionClosed)))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn zero_budget_queued_close_cannot_stall_before_read() {
                let (mut io, _peer) = connection(false, false);
                io.block_write = true;
                let writes = io.writes.clone();
                let mut cfg = config();
                cfg.close_timeout = 0;
                let mut ws = ($make)(io, cfg);
                ws.feed(Message::Close(None)).await.unwrap();
                assert!(matches!(
                    futures_util::poll!(ws.next()),
                    Poll::Ready(Some(Err(Error::ConnectionClosed)))
                ));
                assert!(ws.next().await.is_none());
                assert!(ws.flush().await.is_err());
                assert_eq!(writes.get(), 2);
            }

            #[tokio::test(start_paused = true)]
            async fn accepted_batch_survives_close_deadline() {
                let (io, mut peer) = connection(true, false);
                let mut ws = ($make)(io, config());
                ws.close(1000, "").await.unwrap();
                peer.write_all(b"\x89\x01p\x82\x01d\x88\x02\x03\xe8")
                    .await
                    .unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                tokio::time::advance(Duration::from_secs(2)).await;
                assert!(matches!(ws.next().await, Some(Ok(Message::Binary(_)))));
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn zero_budget_local_close_reads_one_ready_batch() {
                let (io, mut peer) = connection(true, false);
                let mut cfg = config();
                cfg.close_timeout = 0;
                let mut ws = ($make)(io, cfg);
                ws.close(1000, "").await.unwrap();
                peer.write_all(b"\x89\x01p\x88\x02\x03\xe8").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn zero_budget_read_attempt_is_not_renewed() {
                let (io, mut peer) = connection(false, false);
                let mut cfg = config();
                cfg.close_timeout = 0;
                let mut ws = ($make)(io, cfg);
                ws.close(1000, "").await.unwrap();
                peer.write_all(b"\x89\x01p").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                peer.write_all(b"\x89\x01p").await.unwrap();
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn accepted_close_does_not_wait_for_preceding_pong() {
                let (mut io, mut peer) = connection(true, false);
                io.block_write = true;
                let mut ws = ($make)(io, config());
                peer.write_all(b"\x89\x01p\x88\x02\x03\xe8").await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_secs(2), ws.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .is_ping()
                );
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn shutdown_failure_preserves_peer_close_once() {
                let (io, mut peer) = connection(false, false);
                let mut ws = ($make)(io, config());
                peer.write_all(b"\x88\x02\x03\xe8").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn pending_shutdown_cannot_hold_peer_close_forever() {
                let (io, mut peer) = connection(true, false);
                let mut ws = ($make)(io, config());
                peer.write_all(b"\x88\x02\x03\xe8").await.unwrap();
                let start = tokio::time::Instant::now();
                let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
                    .await
                    .unwrap();
                assert!(msg.unwrap().unwrap().is_close());
                assert_eq!(start.elapsed(), Duration::from_secs(1));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn local_close_bounds_silent_peer_wait() {
                let (io, _peer) = connection(true, false);
                let mut ws = ($make)(io, config());
                ws.close(1000, "").await.unwrap();
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), ws.next())
                        .await
                        .unwrap(),
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn direct_close_and_crossing_pings_share_one_budget() {
                let (io, mut peer) = connection(false, false);
                let mut ws = ($make)(io, config());
                ws.send(Message::Close(None)).await.unwrap();
                for _ in 0..3 {
                    tokio::time::advance(Duration::from_millis(300)).await;
                    peer.write_all(b"\x89\x01p").await.unwrap();
                    assert!(ws.next().await.unwrap().unwrap().is_ping());
                }
                tokio::time::advance(Duration::from_millis(100)).await;
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn cleanup_flush_error_preserves_idle_timeout_once() {
                let (io, _peer) = connection(false, true);
                let mut ws = ($make)(
                    io,
                    Config::builder()
                        .auto_ping(false)
                        .idle_timeout(1)
                        .close_timeout(1)
                        .build(),
                );
                assert!(matches!(ws.next().await, Some(Err(Error::IdleTimeout))));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn zero_budget_reports_a_received_close_despite_pending_shutdown() {
                let (io, mut peer) = connection(true, false);
                let shutdowns = io.shutdowns.clone();
                let mut ws = ($make)(
                    io,
                    Config::builder()
                        .auto_ping(false)
                        .idle_timeout(0)
                        .close_timeout(0)
                        .build(),
                );
                peer.write_all(b"\x88\x02\x03\xe8").await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), ws.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .is_close()
                );
                assert!(ws.next().await.is_none());
                assert_eq!(shutdowns.get(), 1);
            }

            #[tokio::test(start_paused = true)]
            async fn peer_close_shutdown_uses_the_remaining_local_budget() {
                let (io, mut peer) = connection(true, false);
                let mut ws = ($make)(io, config());
                let start = tokio::time::Instant::now();
                ws.close(1000, "").await.unwrap();
                tokio::time::advance(Duration::from_millis(700)).await;
                peer.write_all(b"\x88\x02\x03\xe8").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert_eq!(start.elapsed(), Duration::from_secs(1));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn closing_budget_replaces_an_already_armed_heartbeat() {
                let (io, _peer) = connection(false, false);
                let mut ws = ($make)(
                    io,
                    Config::builder()
                        .idle_timeout(0)
                        .ping_interval(1)
                        .close_timeout(5)
                        .build(),
                );
                assert!(futures_util::poll!(ws.next()).is_pending());
                ws.close(1000, "").await.unwrap();
                tokio::time::advance(Duration::from_secs(1)).await;
                assert!(futures_util::poll!(ws.next()).is_pending());
                tokio::time::advance(Duration::from_secs(4)).await;
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn pending_shutdown_preserves_pong_timeout() {
                let (io, _peer) = connection(true, false);
                let mut ws = ($make)(
                    io,
                    Config::builder()
                        .idle_timeout(0)
                        .ping_interval(1)
                        .pong_timeout(1)
                        .close_timeout(1)
                        .build(),
                );
                let start = tokio::time::Instant::now();
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::HeartbeatTimeout))
                ));
                assert_eq!(start.elapsed(), Duration::from_secs(3));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn ordinary_pong_flush_error_is_reported_once() {
                let (io, mut peer) = connection(false, true);
                let mut ws = ($make)(io, config());
                peer.write_all(b"\x89\x01p").await.unwrap();
                assert!(matches!(ws.next().await, Some(Err(Error::Io(_)))));
                assert!(ws.next().await.is_none());
            }

            #[tokio::test(start_paused = true)]
            async fn expired_close_write_is_not_restarted() {
                let (mut io, _peer) = connection(false, false);
                io.block_write = true;
                let writes = io.writes.clone();
                let shutdowns = io.shutdowns.clone();
                let mut ws = ($make)(
                    io,
                    Config::builder()
                        .auto_ping(false)
                        .idle_timeout(0)
                        .close_timeout(0)
                        .build(),
                );
                assert!(matches!(
                    ws.close(1000, "").await,
                    Err(Error::ConnectionClosed)
                ));
                assert!(ws.next().await.is_none());
                assert!(SinkExt::flush(&mut ws).await.is_err());
                SinkExt::close(&mut ws).await.unwrap();
                assert_eq!(writes.get(), 2);
                assert_eq!(shutdowns.get(), 0);
            }
        }
    };
}

close_cases!(plain, WebSocketStream::client);
#[cfg(feature = "permessage-deflate")]
close_cases!(compressed, |io, config| {
    sockudo_ws::CompressedWebSocketStream::client(io, config, Default::default())
});
