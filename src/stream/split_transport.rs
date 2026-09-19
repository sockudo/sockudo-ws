//! Shared ownership for a split transport that the driver can release on timeout.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// Like Tokio's generic split, only synchronous I/O polls hold the mutex. The
// optional stream lets the driver release it even while the read handle lives.
pub(super) struct SplitTransport<S> {
    stream: Arc<Mutex<Option<S>>>,
}

impl<S> SplitTransport<S> {
    pub(super) fn pair(stream: S) -> (Self, Self) {
        let reader = Self {
            stream: Arc::new(Mutex::new(Some(stream))),
        };
        let writer = reader.clone();
        (reader, writer)
    }

    pub(super) fn close_with(&self, publish_terminal: impl FnOnce()) {
        let mut stream = self.stream.lock().unwrap();
        // Keep reads excluded until both destruction and terminal publication
        // finish: a concurrent reader must see the typed cause before EOF.
        drop(stream.take());
        publish_terminal();
    }
}

impl<S> Clone for SplitTransport<S> {
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SplitTransport<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.stream.lock().unwrap().as_mut() {
            Some(stream) => Pin::new(stream).poll_read(cx, buf),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SplitTransport<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.stream.lock().unwrap().as_mut() {
            Some(stream) => Pin::new(stream).poll_write(cx, buf),
            // A direct application writer may be polled again after the driver
            // releases the transport. Its shared state supplies the typed cause.
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.stream.lock().unwrap().as_mut() {
            Some(stream) => Pin::new(stream).poll_flush(cx),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.stream.lock().unwrap().as_mut() {
            Some(stream) => Pin::new(stream).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}
