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
