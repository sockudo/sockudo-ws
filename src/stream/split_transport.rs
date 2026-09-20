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

impl<S: AsyncWrite + Unpin> SplitTransport<S> {
    pub(super) async fn write_all_and_flush(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        std::future::poll_fn(|cx| {
            loop {
                let mut stream = self.stream.lock().unwrap();
                let stream = stream
                    .as_mut()
                    .expect("write after split transport release");
                if written == bytes.len() {
                    return Pin::new(stream).poll_flush(cx);
                }
                match Pin::new(&mut *stream).poll_write(cx, &bytes[written..]) {
                    Poll::Ready(Ok(0)) => {
                        return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                    }
                    Poll::Ready(Ok(count)) => {
                        written += count;
                        if written == bytes.len() {
                            return Pin::new(stream).poll_flush(cx);
                        }
                        // Match write_all's lock boundary so a reader can make
                        // progress between immediately ready partial writes.
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await
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
        let mut stream = self.stream.lock().unwrap();
        // The driver drops each write future before starting another operation
        // after termination; writing a released transport is a driver bug.
        Pin::new(
            stream
                .as_mut()
                .expect("write after split transport release"),
        )
        .poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut stream = self.stream.lock().unwrap();
        Pin::new(
            stream
                .as_mut()
                .expect("flush after split transport release"),
        )
        .poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut stream = self.stream.lock().unwrap();
        Pin::new(
            stream
                .as_mut()
                .expect("shutdown after split transport release"),
        )
        .poll_shutdown(cx)
    }
}
