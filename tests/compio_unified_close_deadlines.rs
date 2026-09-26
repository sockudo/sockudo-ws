#![cfg(feature = "compio-runtime")]

use std::cell::Cell;
use std::future::pending;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use sockudo_ws::{CompioWebSocketStream, Config, Error, Message};

struct TestIo {
    input: Option<&'static [u8]>,
    pending_shutdown: bool,
    fail_flush: bool,
    block_write: bool,
    block_after_local_close: bool,
    repeat_ping: bool,
    writes: Rc<Cell<usize>>,
}

impl TestIo {
    fn new(input: Option<&'static [u8]>, pending_shutdown: bool) -> Self {
        Self {
            input,
            pending_shutdown,
            fail_flush: false,
            block_write: false,
            block_after_local_close: false,
            repeat_ping: false,
            writes: Rc::new(Cell::new(0)),
        }
    }
}

impl AsyncRead for TestIo {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        if self.repeat_ping {
            return io::Cursor::new(b"\x89\x01p").read(buf).await;
        }
        match self.input.take() {
            Some(bytes) => io::Cursor::new(bytes).read(buf).await,
            None => pending().await,
        }
    }
}

impl AsyncWrite for TestIo {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.writes.set(self.writes.get() + 1);
        if self.block_after_local_close && self.writes.get() > 1 {
            return pending().await;
        }
        if self.block_write {
            return if self.writes.get() == 1 {
                BufResult(Ok(buf.as_init().len().min(3)), buf)
            } else {
                pending().await
            };
        }
        BufResult(Ok(buf.as_init().len()), buf)
    }
    async fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            Err(io::Error::other("flush failed"))
        } else {
            Ok(())
        }
    }
    async fn shutdown(&mut self) -> io::Result<()> {
        if self.pending_shutdown {
            pending().await
        } else {
            Err(io::Error::other("shutdown failed"))
        }
    }
}

fn config(seconds: u32) -> Config {
    Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(seconds)
        .build()
}

macro_rules! close_cases {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;

            #[compio::test]
            async fn post_expiry_read_is_not_renewed_after_pre_expiry_input() {
                let mut io = TestIo::new(None, false);
                io.repeat_ping = true;
                let mut ws = ($make)(io, config(1));
                ws.close(1000, "").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                compio::time::sleep(Duration::from_millis(1100)).await;
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn pong_write_timeout_terminates_without_draining_data() {
                let mut io = TestIo::new(Some(b"\x89\x01p\x82\x01d"), false);
                io.block_after_local_close = true;
                let mut ws = ($make)(io, config(1));
                ws.close(1000, "").await.unwrap();
                assert!(matches!(
                    compio::time::timeout(Duration::from_secs(3), ws.next())
                        .await
                        .unwrap(),
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn accepted_close_before_parse_error_suppresses_all_pongs() {
                let io = TestIo::new(Some(b"\x89\x01p\x89\x01q\x88\x02\x03\xe8\x83\x00"), false);
                let writes = io.writes.clone();
                let mut ws = ($make)(io, config(1));
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert_eq!(writes.get(), 0);
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn expired_nonzero_budget_reads_ready_close_once() {
                let mut ws = ($make)(TestIo::new(Some(b"\x88\x02\x03\xe8"), true), config(1));
                ws.close(1000, "").await.unwrap();

                compio::time::sleep(Duration::from_millis(1100)).await;
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn zero_budget_tcp_close_does_not_wait_for_driver_completion() {
                let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let client = compio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let (_peer, _) = listener.accept().await.unwrap();
                let mut ws = ($make)(client, config(0));
                let driver = compio::runtime::Runtime::with_current(|rt| rt.driver_type());
                let result = futures_util::poll!(std::pin::pin!(ws.close(1000, "")));
                eprintln!("zero-budget TCP close: driver={driver:?}, result={result:?}");
                // Submission is not completion on every driver. Either outcome
                // is valid, but zero budget must not wait for the driver's turn.
                assert!(matches!(
                    result,
                    std::task::Poll::Ready(Ok(()) | Err(Error::ConnectionClosed))
                ));
                assert!(matches!(
                    futures_util::poll!(std::pin::pin!(ws.next())),
                    std::task::Poll::Ready(None | Some(Err(Error::ConnectionClosed)))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn accepted_batch_survives_close_deadline() {
                let mut ws = ($make)(
                    TestIo::new(Some(b"\x89\x01p\x82\x01d\x88\x02\x03\xe8"), true),
                    config(1),
                );
                ws.close(1000, "").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                compio::time::sleep(Duration::from_millis(1100)).await;
                assert!(matches!(ws.next().await, Some(Ok(Message::Binary(_)))));
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn zero_budget_local_close_reads_one_ready_batch() {
                let mut ws = ($make)(
                    TestIo::new(Some(b"\x89\x01p\x88\x02\x03\xe8"), true),
                    config(0),
                );
                ws.close(1000, "").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn zero_budget_read_attempt_is_not_renewed() {
                let mut io = TestIo::new(None, false);
                io.repeat_ping = true;
                let mut ws = ($make)(io, config(0));
                ws.close(1000, "").await.unwrap();
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn accepted_close_does_not_wait_for_preceding_pong() {
                let mut io = TestIo::new(Some(b"\x89\x01p\x88\x02\x03\xe8"), true);
                io.block_write = true;
                let mut ws = ($make)(io, config(1));
                assert!(
                    compio::time::timeout(Duration::from_secs(2), ws.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .is_ping()
                );
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn shutdown_failure_preserves_peer_close_once() {
                let mut ws = ($make)(TestIo::new(Some(b"\x88\x02\x03\xe8"), false), config(1));
                assert!(ws.next().await.unwrap().unwrap().is_close());
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn pending_shutdown_cannot_hold_peer_close_forever() {
                let mut ws = ($make)(TestIo::new(Some(b"\x88\x02\x03\xe8"), true), config(1));
                let start = Instant::now();
                assert!(
                    compio::time::timeout(Duration::from_secs(3), ws.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .is_close()
                );
                assert!(start.elapsed() >= Duration::from_millis(900));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn local_close_bounds_silent_peer_wait() {
                let mut ws = ($make)(TestIo::new(None, true), config(1));
                ws.close(1000, "").await.unwrap();
                assert!(matches!(
                    compio::time::timeout(Duration::from_secs(3), ws.next())
                        .await
                        .unwrap(),
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn crossing_pings_do_not_extend_direct_close_budget() {
                let mut io = TestIo::new(None, false);
                io.repeat_ping = true;
                let mut ws = ($make)(io, config(1));
                ws.send(Message::Close(None)).await.unwrap();
                for _ in 0..3 {
                    compio::time::sleep(Duration::from_millis(250)).await;
                    assert!(ws.next().await.unwrap().unwrap().is_ping());
                }
                compio::time::sleep(Duration::from_millis(300)).await;
                // All budgets permit exactly one ready read after expiry.
                assert!(ws.next().await.unwrap().unwrap().is_ping());
                assert!(matches!(
                    ws.next().await,
                    Some(Err(Error::ConnectionClosed))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn cleanup_failure_preserves_idle_timeout() {
                let mut io = TestIo::new(None, false);
                io.fail_flush = true;
                let cfg = Config::builder()
                    .auto_ping(false)
                    .idle_timeout(1)
                    .close_timeout(1)
                    .build();
                let mut ws = ($make)(io, cfg);
                assert!(matches!(
                    compio::time::timeout(Duration::from_secs(3), ws.next())
                        .await
                        .unwrap(),
                    Some(Err(Error::IdleTimeout))
                ));
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn zero_budget_preserves_close_despite_pending_shutdown() {
                let mut ws = ($make)(TestIo::new(Some(b"\x88\x02\x03\xe8"), true), config(0));
                assert!(
                    compio::time::timeout(Duration::from_secs(1), ws.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .is_close()
                );
                assert!(ws.next().await.is_none());
            }

            #[compio::test]
            async fn cancelled_close_write_is_never_restarted() {
                let mut io = TestIo::new(None, false);
                io.block_write = true;
                let writes = io.writes.clone();
                let mut ws = ($make)(io, config(0));
                assert!(matches!(
                    ws.close(1000, "").await,
                    Err(Error::ConnectionClosed)
                ));
                assert!(ws.next().await.is_none());
                assert!(ws.send(Message::text("late")).await.is_err());
                assert!(ws.flush().await.is_err());
                assert_eq!(writes.get(), 2);
            }
        }
    };
}

close_cases!(plain, CompioWebSocketStream::client);
#[cfg(feature = "permessage-deflate")]
close_cases!(compressed, |io, config| {
    sockudo_ws::compio::CompioCompressedWebSocketStream::client(io, config, Default::default())
});
