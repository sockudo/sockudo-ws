#![cfg(feature = "tokio-runtime")]

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::StreamExt;
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct InitializingReader {
    wire: &'static [u8],
    pending: bool,
}

impl AsyncRead for InitializingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let Some((&byte, rest)) = self.wire.split_first() else {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset)));
        };
        // AsyncRead implementations may initialize and read the unfilled region.
        buf.initialize_unfilled().fill(0);
        buf.put_slice(&[byte]);
        self.wire = rest;
        self.pending = true;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for InitializingReader {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn initialized_reads_preserve_partial_frames_across_pending() {
    let io = InitializingReader {
        wire: b"\x81\x03abc",
        pending: true,
    };
    let mut ws = WebSocketStream::client(io, Config::default());
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"abc");
    assert!(matches!(
        ws.next().await.unwrap(),
        Err(sockudo_ws::Error::ConnectionReset)
    ));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_stream_preserves_partial_frames_across_pending() {
    let io = InitializingReader {
        wire: b"\x81\x03abc",
        pending: true,
    };
    let mut ws = sockudo_ws::CompressedWebSocketStream::client(
        io,
        Config::default(),
        sockudo_ws::deflate::DeflateConfig::default(),
    );
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"abc");
    assert!(matches!(
        ws.next().await.unwrap(),
        Err(sockudo_ws::Error::ConnectionReset)
    ));
}
