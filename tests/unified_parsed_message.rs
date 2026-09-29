#![cfg(all(feature = "tokio-runtime", not(feature = "test-util")))]

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use sockudo_ws::{Config, Error, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct CrossingDeadlineIo {
    clock: Arc<quanta::Mock>,
    blocked: Arc<AtomicBool>,
    received: bool,
}

impl AsyncRead for CrossingDeadlineIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.received {
            return Poll::Pending;
        }
        self.received = true;
        // Cross the logical deadline between the initial check and delivery.
        self.clock.increment(Duration::from_secs(1));
        buffer.put_slice(b"\x82\x01a");
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for CrossingDeadlineIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.blocked.load(Ordering::Relaxed) {
            Poll::Pending
        } else {
            Poll::Ready(Ok(bytes.len()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn parsed_data_survives_cancelled_ping_flush(#[case] split_after_pending: bool) {
    let (clock, mock) = quanta::Clock::mock();
    let blocked = Arc::new(AtomicBool::new(true));
    let io = CrossingDeadlineIo {
        clock: mock,
        blocked: blocked.clone(),
        received: false,
    };
    let config = Config::builder()
        .auto_ping(true)
        .ping_interval(1)
        .pong_timeout(10)
        .idle_timeout(0)
        .build();
    let mut stream = quanta::with_clock(&clock, || WebSocketStream::client(io, config));
    let mut context = Context::from_waker(Waker::noop());
    {
        let next = stream.next();
        tokio::pin!(next);
        assert!(quanta::with_clock(&clock, || next.as_mut().poll(&mut context)).is_pending());
    }
    blocked.store(false, Ordering::Relaxed);

    if split_after_pending {
        let (mut reader, _writer) = quanta::with_clock(&clock, || stream.split());
        {
            let next = reader.next();
            tokio::pin!(next);
            let Poll::Ready(Some(Ok(message))) =
                quanta::with_clock(&clock, || next.as_mut().poll(&mut context))
            else {
                panic!("split must retain the parsed data");
            };
            assert_eq!(message.as_bytes(), b"a");
        }
        let next = reader.next();
        tokio::pin!(next);
        assert!(quanta::with_clock(&clock, || next.as_mut().poll(&mut context)).is_pending());
    } else {
        let Poll::Ready(Some(Ok(message))) =
            quanta::with_clock(&clock, || Pin::new(&mut stream).poll_next(&mut context))
        else {
            panic!("cancelled next must retain the parsed data");
        };
        assert_eq!(message.as_bytes(), b"a");
        assert!(
            quanta::with_clock(&clock, || Pin::new(&mut stream).poll_next(&mut context))
                .is_pending()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn idle_expiry_after_read_precedes_parsed_data() {
    let (clock, mock) = quanta::Clock::mock();
    let io = CrossingDeadlineIo {
        clock: mock,
        blocked: Arc::new(AtomicBool::new(false)),
        received: false,
    };
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let mut stream = quanta::with_clock(&clock, || WebSocketStream::client(io, config));
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(
        quanta::with_clock(&clock, || Pin::new(&mut stream).poll_next(&mut context)),
        Poll::Ready(Some(Err(Error::IdleTimeout)))
    ));
    assert!(matches!(
        quanta::with_clock(&clock, || Pin::new(&mut stream).poll_next(&mut context)),
        Poll::Ready(None)
    ));
}
