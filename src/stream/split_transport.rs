//! Shared ownership for a split transport that the driver can release on timeout.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct SharedTransport<S> {
    state: Mutex<TransportState<S>>,
    is_write_vectored: bool,
}

enum TransportState<S> {
    Open(S),
    // Public I/O polls intentionally stay pending without storing a waker. The
    // split reader and writer also await terminal and cancellation signals.
    Closing,
    Closed,
}

// Like Tokio's generic split, only synchronous I/O polls hold the mutex. The
// terminal states let the driver release the stream while either handle lives.
pub(super) struct SplitTransport<S> {
    shared: Arc<SharedTransport<S>>,
}

impl<S> SplitTransport<S> {
    pub(super) fn pair(stream: S) -> (Self, Self)
    where
        S: AsyncWrite,
    {
        let is_write_vectored = stream.is_write_vectored();
        let reader = Self {
            shared: Arc::new(SharedTransport {
                state: Mutex::new(TransportState::Open(stream)),
                is_write_vectored,
            }),
        };
        let writer = reader.clone();
        (reader, writer)
    }

    /// Take exclusive ownership while an orderly terminal shutdown is pending.
    ///
    /// Polls through the public halves remain pending until `finish_close`
    /// publishes the terminal cause and wakes them through their existing
    /// terminal/cancellation signals.
    pub(super) fn begin_close(&self) -> S {
        let mut state = self.shared.state.lock().unwrap();
        match std::mem::replace(&mut *state, TransportState::Closing) {
            TransportState::Open(stream) => stream,
            TransportState::Closing | TransportState::Closed => {
                unreachable!("the split driver closes a transport only once")
            }
        }
    }

    pub(super) fn finish_close(&self, stream: S, publish_terminal: impl FnOnce()) {
        drop(stream);
        let mut state = self.shared.state.lock().unwrap();
        assert!(matches!(*state, TransportState::Closing));
        // Keep reads pending until destruction and terminal publication both
        // finish, so a concurrent reader observes the typed cause after drop.
        *state = TransportState::Closed;
        publish_terminal();
    }

    pub(super) fn close_with(&self, publish_terminal: impl FnOnce()) {
        let stream = self.begin_close();
        self.finish_close(stream, publish_terminal);
    }
}

impl<S> Clone for SplitTransport<S> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SplitTransport<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self.shared.state.lock().unwrap() {
            TransportState::Open(stream) => Pin::new(stream).poll_read(cx, buf),
            TransportState::Closing => Poll::Pending,
            TransportState::Closed => Poll::Ready(Ok(())),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SplitTransport<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self.shared.state.lock().unwrap() {
            TransportState::Open(stream) => Pin::new(stream).poll_write(cx, buf),
            TransportState::Closing => Poll::Pending,
            TransportState::Closed => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self.shared.state.lock().unwrap() {
            TransportState::Open(stream) => Pin::new(stream).poll_flush(cx),
            TransportState::Closing => Poll::Pending,
            TransportState::Closed => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self.shared.state.lock().unwrap() {
            TransportState::Open(stream) => Pin::new(stream).poll_shutdown(cx),
            TransportState::Closing => Poll::Pending,
            TransportState::Closed => Poll::Ready(Ok(())),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut *self.shared.state.lock().unwrap() {
            TransportState::Open(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            TransportState::Closing => Poll::Pending,
            TransportState::Closed => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.shared.is_write_vectored
    }
}
