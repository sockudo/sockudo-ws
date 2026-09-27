#![cfg(feature = "tokio-runtime")]

use std::{
    io::{self, IoSlice},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_util::SinkExt;
use sockudo_ws::{Config, Error, Message, Role, WebSocketStream, protocol::Protocol};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct Written {
    bytes: Vec<u8>,
    scalar: usize,
    vectored: usize,
    flushed: bool,
}

struct Probe {
    written: Arc<Mutex<Written>>,
    vectored: bool,
    pending: bool,
    flush_pending: bool,
    terminal: Option<io::Result<usize>>,
}

impl Probe {
    fn write(&mut self, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<usize>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if let Some(result) = self.terminal.take() {
            return Poll::Ready(result);
        }
        self.pending = true;
        let mut written = self.written.lock().unwrap();
        let before = written.bytes.len();
        for buf in bufs {
            let n = buf.len().min(17 - (written.bytes.len() - before));
            written.bytes.extend_from_slice(&buf[..n]);
            if written.bytes.len() - before == 17 {
                break;
            }
        }
        Poll::Ready(Ok(written.bytes.len() - before))
    }
}

impl AsyncRead for Probe {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for Probe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.written.lock().unwrap().scalar += 1;
        self.write(cx, &[IoSlice::new(buf)])
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.written.lock().unwrap().vectored += 1;
        if self.vectored {
            self.write(cx, bufs)
        } else {
            self.write(cx, &bufs[..1])
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.vectored
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.flush_pending {
            self.flush_pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.written.lock().unwrap().flushed = true;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn setup(
    vectored: bool,
    terminal: Option<io::Result<usize>>,
) -> (Probe, Arc<Mutex<Written>>, Config) {
    let written = Arc::new(Mutex::new(Written::default()));
    let io = Probe {
        written: written.clone(),
        vectored,
        pending: true,
        flush_pending: true,
        terminal,
    };
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .max_backpressure(usize::MAX)
        .build();
    (io, written, config)
}

macro_rules! write_cases {
    ($module:ident, $make:expr, $segments:expr) => {
        mod $module {
            use super::*;

            #[rstest::rstest]
            #[tokio::test]
            async fn partial_writes_preserve_order_and_dispatch(
                #[values(false, true)] vectored: bool,
                #[values(false, true)] close: bool,
                #[values(32, 8192)] size: usize,
            ) {
                let (io, written, config) = setup(vectored, None);
                let mut ws = ($make)(io, config);
                let payload = Bytes::from(vec![0x5a; size]);
                for bytes in [
                    Bytes::from_static(b"before"),
                    payload.clone(),
                    Bytes::from_static(b"after"),
                ] {
                    ws.feed(Message::Binary(bytes)).await.unwrap();
                }

                // close() exercises the async flush; Sink::flush exercises poll_write_out.
                if close {
                    ws.close(1000, "done").await.unwrap();
                } else {
                    ws.flush().await.unwrap();
                }

                let written = written.lock().unwrap();
                assert!(written.flushed);
                assert_eq!(written.vectored > 0, $segments && size >= 8192 && vectored);
                if !vectored || !$segments || size < 8192 {
                    assert!(written.scalar > 0);
                }
                let mut wire = BytesMut::from(written.bytes.as_slice());
                let messages = Protocol::new(Role::Client, 65536, 65536)
                    .process(&mut wire)
                    .unwrap();
                assert!(wire.is_empty());
                assert_eq!(messages.len(), if close { 4 } else { 3 });
                assert_eq!(messages[0].as_bytes(), b"before");
                assert_eq!(messages[1].as_bytes(), payload.as_ref());
                assert_eq!(messages[2].as_bytes(), b"after");
                if close {
                    assert!(matches!(
                        &messages[3],
                        Message::Close(Some(reason))
                            if reason.code == 1000 && reason.reason == "done"
                    ));
                }
            }

            #[rstest::rstest]
            #[tokio::test]
            async fn zero_and_error_writes_remain_terminal(
                #[values(false, true)] close: bool,
                #[values(false, true)] error: bool,
            ) {
                let terminal = if error {
                    Err(io::Error::from(io::ErrorKind::Other))
                } else {
                    Ok(0)
                };
                let (io, written, config) = setup(true, Some(terminal));
                let mut ws = ($make)(io, config);
                ws.feed(Message::binary(vec![1; 32])).await.unwrap();

                let result = if close {
                    ws.close(1000, "done").await
                } else {
                    ws.flush().await
                };

                if error {
                    assert!(matches!(
                        result,
                        Err(Error::Io(error)) if error.kind() == io::ErrorKind::Other
                    ));
                } else {
                    assert!(matches!(result, Err(Error::ConnectionClosed)));
                }
                assert!(written.lock().unwrap().bytes.is_empty());
            }
        }
    };
}

write_cases!(plain, WebSocketStream::server, true);

#[rstest::rstest]
#[tokio::test]
async fn vectored_zero_and_error_writes_remain_terminal(
    #[values(false, true)] close: bool,
    #[values(false, true)] error: bool,
) {
    let terminal = if error {
        Err(io::Error::from(io::ErrorKind::Other))
    } else {
        Ok(0)
    };
    let (io, written, config) = setup(true, Some(terminal));
    let mut ws = WebSocketStream::server(io, config);
    // Large server payloads keep the header and payload in separate segments.
    ws.feed(Message::binary(vec![1; 8192])).await.unwrap();

    let result = if close {
        ws.close(1000, "done").await
    } else {
        ws.flush().await
    };

    if error {
        assert!(matches!(result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::Other));
    } else {
        assert!(matches!(result, Err(Error::ConnectionClosed)));
    }
    let written = written.lock().unwrap();
    assert!(written.vectored > 0);
    assert_eq!(written.scalar, 0);
    assert!(written.bytes.is_empty());
}

#[cfg(feature = "permessage-deflate")]
write_cases!(
    compressed,
    |io, config| sockudo_ws::CompressedWebSocketStream::server(
        io,
        config,
        sockudo_ws::DeflateConfig {
            compression_threshold: usize::MAX,
            ..Default::default()
        }
    ),
    false
);
