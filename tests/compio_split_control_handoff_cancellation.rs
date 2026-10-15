#![cfg(feature = "compio-runtime")]

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::time::Duration;

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, util::Splittable};
use futures_util::FutureExt;
use sockudo_ws::{CompioWebSocketStream, Config, Message};

// Control-frame counts around and beyond the split control queue capacity, so
// that one count places the cancelled hand-off on the trailing Close or parse
// error.
const CONTROL_COUNTS: std::ops::RangeInclusive<usize> = 1..=64;
const PING: [u8; 2] = [0x89, 0x00];
const PONG: [u8; 2] = [0x8a, 0x00];
const CLOSE_1000: [u8; 4] = [0x88, 0x02, 0x03, 0xe8];
const RESERVED_OPCODE: [u8; 2] = [0x83, 0x00];

/// Delivers the whole burst in its first read and never completes another, so
/// every pending `next()` after that read waits on the control hand-off.
#[derive(Clone)]
struct BurstIo {
    input: Rc<RefCell<Option<Vec<u8>>>>,
    reads: Rc<Cell<usize>>,
    written: Rc<RefCell<Vec<u8>>>,
}

impl BurstIo {
    fn new(burst: Vec<u8>) -> Self {
        Self {
            input: Rc::new(RefCell::new(Some(burst))),
            reads: Rc::new(Cell::new(0)),
            written: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Reports whether the client wrote a (masked) Close frame.
    fn wrote_close(&self) -> bool {
        let written = self.written.borrow();
        let mut at = 0;
        while at + 2 <= written.len() {
            if written[at] & 0x0f == 0x08 {
                return true;
            }
            at += 2 + 4 + usize::from(written[at + 1] & 0x7f);
        }
        false
    }
}

impl AsyncRead for BurstIo {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.reads.set(self.reads.get() + 1);
        let input = self.input.borrow_mut().take();
        match input {
            Some(input) => io::Cursor::new(input).read(buf).await,
            None => std::future::pending().await,
        }
    }
}

impl AsyncWrite for BurstIo {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.written.borrow_mut().extend_from_slice(buf.as_init());
        BufResult(Ok(buf.as_init().len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Splittable for BurstIo {
    type ReadHalf = Self;
    type WriteHalf = Self;

    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (self.clone(), self)
    }
}

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

fn burst(frame: [u8; 2], count: usize, tail: &[u8]) -> BurstIo {
    let mut wire = frame.repeat(count);
    wire.extend_from_slice(tail);
    BurstIo::new(wire)
}

async fn close_written(io: &BurstIo) -> bool {
    compio::time::timeout(Duration::from_secs(5), async {
        while !io.wrote_close() {
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .is_ok()
}

// The first call is awaited so the burst is read; later calls are polled
// without yielding, so the control driver cannot drain its queue, and each call
// that waits is dropped, as select! or timeout would. Every cancelled call is
// followed by one awaited call, which must neither hang nor skip input.
macro_rules! receive_with_cancellation {
    ($reader:expr) => {{
        let mut received = vec![$reader.next().await.unwrap()];
        let mut cancelled = 0;
        while !matches!(received.last(), Some(Ok(Message::Close(_)) | Err(_))) {
            let item = match $reader.next().now_or_never() {
                Some(item) => item,
                None => {
                    cancelled += 1;
                    compio::time::timeout(Duration::from_secs(5), $reader.next())
                        .await
                        .expect("next() must resume a cancelled control hand-off")
                }
            };
            let Some(item) = item else { break };
            received.push(item);
        }
        (received, cancelled)
    }};
}

fn controls(received: &[sockudo_ws::Result<Message>]) -> usize {
    received
        .iter()
        .filter(|item| matches!(item, Ok(Message::Ping(_) | Message::Pong(_))))
        .count()
}

macro_rules! handoff_cases {
    ($module:ident, $split:expr) => {
        mod $module {
            use super::*;

            #[compio::test]
            async fn cancelled_handoff_keeps_controls_and_peer_close() {
                let mut cancelled_any = false;
                for frame in [PING, PONG] {
                    for count in CONTROL_COUNTS {
                        let io = burst(frame, count, &CLOSE_1000);
                        let (mut reader, _writer) = ($split)(io.clone());

                        let (received, cancelled) = receive_with_cancellation!(reader);

                        cancelled_any |= cancelled > 0;
                        assert_eq!(io.reads.get(), 1, "{frame:x?} x {count}");
                        assert_eq!(controls(&received), count, "{frame:x?} x {count}");
                        assert!(matches!(
                            received.last(),
                            Some(Ok(Message::Close(Some(reason)))) if reason.code == 1000
                        ));
                        assert_eq!(received.len(), count + 1, "{frame:x?} x {count}");
                        assert!(close_written(&io).await, "{frame:x?} x {count}");
                    }
                }
                assert!(cancelled_any, "the fixture must cancel a waiting hand-off");
            }

            #[compio::test]
            async fn cancelled_handoff_keeps_parse_error() {
                let mut cancelled_any = false;
                for frame in [PING, PONG] {
                    for count in CONTROL_COUNTS {
                        let io = burst(frame, count, &RESERVED_OPCODE);
                        let (mut reader, _writer) = ($split)(io.clone());

                        let (received, cancelled) = receive_with_cancellation!(reader);

                        cancelled_any |= cancelled > 0;
                        assert_eq!(io.reads.get(), 1, "{frame:x?} x {count}");
                        assert_eq!(controls(&received), count, "{frame:x?} x {count}");
                        assert!(matches!(received.last(), Some(Err(_))));
                        assert_eq!(received.len(), count + 1, "{frame:x?} x {count}");
                        let after = compio::time::timeout(Duration::from_secs(5), reader.next())
                            .await
                            .expect("the reader must end after reporting a parse error");
                        assert!(after.is_none());
                    }
                }
                assert!(cancelled_any, "the fixture must cancel a waiting hand-off");
            }

            #[compio::test]
            async fn waiting_handoff_ends_when_the_driver_stops() {
                let io = burst(PING, *CONTROL_COUNTS.end(), &CLOSE_1000);
                let (mut reader, writer) = ($split)(io);
                reader.next().await.unwrap().unwrap();
                while reader.next().now_or_never().is_some() {}

                // Dropping either half cancels the control driver.
                drop(writer);

                compio::time::timeout(Duration::from_secs(5), async {
                    while reader.next().await.is_some() {}
                })
                .await
                .expect("a waiting hand-off must observe the stopped driver");
            }
        }
    };
}

handoff_cases!(plain, |io| CompioWebSocketStream::client(io, config())
    .split());

#[cfg(feature = "permessage-deflate")]
handoff_cases!(compressed, |io| {
    sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    )
    .split()
});
