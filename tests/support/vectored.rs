use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct WriteProbe {
    vectored: bool,
    pending: bool,
    terminal: Option<io::Result<usize>>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl AsyncRead for WriteProbe {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for WriteProbe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[IoSlice::new(data)])
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if let Some(result) = self.terminal.take() {
            return Poll::Ready(result);
        }
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let mut output = self.output.lock().unwrap();
        let before = output.len();
        for buf in bufs.iter().filter(|buf| !buf.is_empty()) {
            let count = buf.len().min(4 - (output.len() - before));
            output.extend_from_slice(&buf[..count]);
            if !self.vectored || output.len() - before == 4 {
                break;
            }
        }
        Poll::Ready(Ok(output.len() - before))
    }

    fn is_write_vectored(&self) -> bool {
        self.vectored
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub fn check_vectored_forwarding<S: AsyncWrite + Unpin>(wrap: impl Fn(WriteProbe) -> S) {
    for vectored in [false, true] {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut stream = wrap(WriteProbe {
            vectored,
            pending: true,
            terminal: None,
            output: output.clone(),
        });
        assert_eq!(stream.is_write_vectored(), vectored);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut bufs = [
            IoSlice::new(b""),
            IoSlice::new(b"ab"),
            IoSlice::new(b"cdefgh"),
        ];
        let mut remaining = &mut bufs[..];
        assert!(
            Pin::new(&mut stream)
                .poll_write_vectored(&mut cx, remaining)
                .is_pending()
        );
        assert!(output.lock().unwrap().is_empty());

        let first = match Pin::new(&mut stream).poll_write_vectored(&mut cx, remaining) {
            Poll::Ready(Ok(count)) => count,
            other => panic!("unexpected write result: {other:?}"),
        };
        assert_eq!(first, if vectored { 4 } else { 2 });
        IoSlice::advance_slices(&mut remaining, first);
        while !remaining.is_empty() {
            let count = match Pin::new(&mut stream).poll_write_vectored(&mut cx, remaining) {
                Poll::Ready(Ok(count)) => count,
                other => panic!("unexpected write result: {other:?}"),
            };
            assert!(count > 0);
            IoSlice::advance_slices(&mut remaining, count);
        }
        assert_eq!(*output.lock().unwrap(), b"abcdefgh");
        for empty in [&[][..], &[IoSlice::new(b"")][..]] {
            assert!(matches!(
                Pin::new(&mut stream).poll_write_vectored(&mut cx, empty),
                Poll::Ready(Ok(0))
            ));
        }
    }
}

/// A wrapper must leave zero writes and transport errors for its caller to handle.
pub fn check_terminal_write_results<S: AsyncWrite + Unpin>(wrap: impl Fn(WriteProbe) -> S) {
    for result in [Ok(0), Err(io::Error::from_raw_os_error(32))] {
        let expected_error = result.as_ref().err().and_then(io::Error::raw_os_error);
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut stream = wrap(WriteProbe {
            vectored: true,
            pending: false,
            terminal: Some(result),
            output: output.clone(),
        });
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let bufs = [IoSlice::new(b"header"), IoSlice::new(b"payload")];
        match Pin::new(&mut stream).poll_write_vectored(&mut cx, &bufs) {
            Poll::Ready(Ok(0)) => assert_eq!(expected_error, None),
            Poll::Ready(Err(error)) if expected_error.is_some() => {
                assert_eq!(error.raw_os_error(), expected_error);
            }
            other => panic!("unexpected terminal write result: {other:?}"),
        }
        assert!(output.lock().unwrap().is_empty());
    }
}
