use std::cell::{Cell, RefCell};
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use tokio::io::AsyncWrite;
use tokio_util::sync::CancellationToken;

use super::write_split_bytes;
use crate::Error;

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct PausingWriter {
    bytes: Vec<u8>,
    released: Rc<Cell<bool>>,
    pause_flush: bool,
    flush_polls: usize,
    io_polls: Rc<Cell<usize>>,
    readiness: Rc<RefCell<Option<Waker>>>,
}

impl AsyncWrite for PausingWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.io_polls.set(self.io_polls.get() + 1);
        if !self.pause_flush && !self.released.get() && !self.bytes.is_empty() {
            *self.readiness.borrow_mut() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let count = if self.pause_flush || self.released.get() {
            bytes.len()
        } else {
            bytes.len().min(2)
        };
        self.bytes.extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.io_polls.set(self.io_polls.get() + 1);
        self.flush_polls += 1;
        if self.pause_flush && !self.released.get() {
            *self.readiness.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[rstest::rstest]
#[case::partial_write(false, b"ab".as_slice())]
#[case::flush(true, b"abcdef".as_slice())]
#[tokio::test]
async fn cancellation_wakes_a_blocked_split_write_without_more_io(
    #[case] pause_flush: bool,
    #[case] expected: &[u8],
) {
    let io_polls = Rc::new(Cell::new(0));
    let mut writer = PausingWriter {
        bytes: Vec::new(),
        released: Rc::new(Cell::new(false)),
        pause_flush,
        flush_polls: 0,
        io_polls: io_polls.clone(),
        readiness: Rc::default(),
    };
    let cancel = CancellationToken::new();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    {
        let mut write = std::pin::pin!(write_split_bytes(&mut writer, b"abcdef", &cancel));
        assert!(std::future::Future::poll(write.as_mut(), &mut cx).is_pending());
        let polls_before_cancel = io_polls.get();

        cancel.cancel();

        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(
            std::future::Future::poll(write.as_mut(), &mut cx),
            Poll::Ready(Err(Error::ConnectionClosed))
        ));
        assert_eq!(io_polls.get(), polls_before_cancel);
    }
    assert_eq!(writer.bytes, expected);
}

#[tokio::test]
async fn resumed_split_write_keeps_its_accepted_prefix() {
    let released = Rc::new(Cell::new(false));
    let readiness = Rc::new(RefCell::new(None));
    let mut writer = PausingWriter {
        bytes: Vec::new(),
        released: released.clone(),
        pause_flush: false,
        flush_polls: 0,
        io_polls: Rc::new(Cell::new(0)),
        readiness: readiness.clone(),
    };
    let cancel = CancellationToken::new();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    {
        let mut write = std::pin::pin!(write_split_bytes(&mut writer, b"abcdef", &cancel));
        assert!(std::future::Future::poll(write.as_mut(), &mut cx).is_pending());

        released.set(true);
        readiness.borrow_mut().take().unwrap().wake();

        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        assert!(matches!(
            std::future::Future::poll(write.as_mut(), &mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    assert_eq!(writer.bytes, b"abcdef");
    assert_eq!(writer.flush_polls, 1);
}
