#![cfg(feature = "tokio-runtime")]

use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures_util::{Sink, SinkExt};
use sockudo_ws::protocol::Protocol;
use sockudo_ws::{Config, Error, Message, Role, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct ShortWriter {
    output: Rc<RefCell<Vec<u8>>>,
    max_write: usize,
    pending: bool,
}

impl ShortWriter {
    fn new(max_write: usize) -> (Self, Rc<RefCell<Vec<u8>>>) {
        let output = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                output: output.clone(),
                max_write,
                pending: true,
            },
            output,
        )
    }
}

impl AsyncRead for ShortWriter {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for ShortWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[io::IoSlice::new(data)])
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        slices: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        // Alternate Pending and short writes to exercise reconstruction of the
        // unwritten suffix across both poll_flush and write().await.
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.pending = true;
        let mut output = self.output.borrow_mut();
        let before = output.len();
        output.extend(
            slices
                .iter()
                .flat_map(|slice| slice.iter().copied())
                .take(self.max_write),
        );
        Poll::Ready(Ok(output.len() - before))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn config() -> Config {
    Config::builder()
        .write_buffer_size(8)
        .auto_ping(false)
        .idle_timeout(0)
        .build()
}

async fn queue_batch<S: Sink<Message, Error = Error> + Unpin>(ws: &mut S) -> Vec<Message> {
    let mut messages = Vec::new();
    // More than 16 frames and more than the configured cork capacity. Stream
    // encoding still uses its contiguous buffer, including compressed frames.
    for index in 0..32 {
        let payload = vec![b'A' + index; 256];
        messages.push(if index % 2 == 0 {
            Message::binary(payload)
        } else {
            Message::text(String::from_utf8(payload).unwrap())
        });
    }
    messages.insert(16, Message::Ping("control".into()));
    for message in &messages {
        ws.feed(message.clone()).await.unwrap();
    }
    messages
}

fn assert_messages(actual: &[Message], expected: &[Message]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            std::mem::discriminant(actual),
            std::mem::discriminant(expected)
        );
        assert_eq!(actual.as_bytes(), expected.as_bytes());
        if let (Message::Close(actual), Message::Close(expected)) = (actual, expected) {
            assert_eq!(
                actual
                    .as_ref()
                    .map(|close| (close.code, close.reason.as_str())),
                expected
                    .as_ref()
                    .map(|close| (close.code, close.reason.as_str()))
            );
        }
    }
}

macro_rules! flush_cases {
    ($module:ident, $compressed:literal, $socket:expr, $decoder:expr) => {
        mod $module {
            use super::*;

            #[tokio::test]
            async fn flush_preserves_frames_across_short_writes_and_pending() {
                let (io, output) = ShortWriter::new(3);
                let mut ws = ($socket)(io);
                let expected = queue_batch(&mut ws).await;

                ws.flush().await.unwrap();

                let mut wire = BytesMut::from(output.borrow().as_slice());
                assert_eq!(wire[0] & 0x40 != 0, $compressed);
                assert_messages(&($decoder).process(&mut wire).unwrap(), &expected);
                assert!(wire.is_empty());
                assert_eq!(ws.write_buffer_len(), 0);
            }

            #[tokio::test]
            async fn close_preserves_queued_frames_across_short_writes_and_pending() {
                let (io, output) = ShortWriter::new(3);
                let mut ws = ($socket)(io);
                let mut expected = queue_batch(&mut ws).await;
                expected.push(Message::Close(Some(sockudo_ws::error::CloseReason::new(
                    1000, "done",
                ))));

                ws.close(1000, "done").await.unwrap();

                let mut wire = BytesMut::from(output.borrow().as_slice());
                assert_eq!(wire[0] & 0x40 != 0, $compressed);
                assert_messages(&($decoder).process(&mut wire).unwrap(), &expected);
                assert!(wire.is_empty());
                assert_eq!(ws.write_buffer_len(), 0);
            }

            #[tokio::test]
            async fn flush_reports_connection_closed_on_zero_write() {
                let (io, _) = ShortWriter::new(0);
                let mut ws = ($socket)(io);
                ws.feed(Message::text("data")).await.unwrap();

                let result = ws.flush().await;

                assert!(matches!(result, Err(Error::ConnectionClosed)));
            }

            #[tokio::test]
            async fn close_reports_connection_closed_on_zero_write() {
                let (io, _) = ShortWriter::new(0);
                let mut ws = ($socket)(io);

                let result = ws.close(1000, "done").await;

                assert!(matches!(result, Err(Error::ConnectionClosed)));
            }
        }
    };
}

flush_cases!(
    plain,
    false,
    |io| WebSocketStream::server(io, config()),
    Protocol::new(Role::Client, 65536, 65536)
);

#[cfg(feature = "permessage-deflate")]
flush_cases!(
    compressed,
    true,
    |io| sockudo_ws::CompressedWebSocketStream::server(
        io,
        config(),
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    sockudo_ws::CompressedProtocol::client(
        65536,
        65536,
        sockudo_ws::deflate::DeflateConfig::default()
    )
);
