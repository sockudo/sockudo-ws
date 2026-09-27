use super::*;

#[derive(Clone, Copy)]
enum ReadCompletion {
    Data,
    Eof,
    Error,
    Pending,
}

struct TerminatingRead {
    shared: Arc<std::sync::OnceLock<Arc<SplitShared>>>,
    completion: ReadCompletion,
}

impl AsyncRead for TerminatingRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Publish after next()'s initial terminal check, while the transport
        // is being polled. This fixes the race order without a timed sleep.
        self.shared
            .get()
            .unwrap()
            .terminate(TerminalCause::IdleTimeout);
        match self.completion {
            ReadCompletion::Data => {
                buf.put_slice(b"\x82\x01a");
                Poll::Ready(Ok(()))
            }
            ReadCompletion::Eof => Poll::Ready(Ok(())),
            ReadCompletion::Error => Poll::Ready(Err(io::Error::other("read failed"))),
            ReadCompletion::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for TerminatingRead {
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

#[rstest::rstest]
#[case::data(ReadCompletion::Data)]
#[case::eof(ReadCompletion::Eof)]
#[case::error(ReadCompletion::Error)]
#[case::pending(ReadCompletion::Pending)]
#[tokio::test]
async fn split_terminal_published_during_read_takes_priority(#[case] completion: ReadCompletion) {
    let shared = Arc::new(std::sync::OnceLock::new());
    let io = TerminatingRead {
        shared: shared.clone(),
        completion,
    };
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();
    assert!(shared.set(reader.shared.clone()).is_ok());

    let result = reader.next().await;

    assert!(matches!(result, Some(Err(Error::IdleTimeout))));
    assert!(reader.next().await.is_none());
}

#[cfg(feature = "permessage-deflate")]
#[rstest::rstest]
#[case::data(ReadCompletion::Data)]
#[case::eof(ReadCompletion::Eof)]
#[case::error(ReadCompletion::Error)]
#[case::pending(ReadCompletion::Pending)]
#[tokio::test]
async fn compressed_split_terminal_published_during_read_takes_priority(
    #[case] completion: ReadCompletion,
) {
    let shared = Arc::new(std::sync::OnceLock::new());
    let io = TerminatingRead {
        shared: shared.clone(),
        completion,
    };
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) =
        CompressedWebSocketStream::client(io, config, crate::DeflateConfig::default()).split();
    assert!(shared.set(reader.shared.clone()).is_ok());

    let result = reader.next().await;

    assert!(matches!(result, Some(Err(Error::IdleTimeout))));
    assert!(reader.next().await.is_none());
}

#[rstest::rstest]
#[case::closed(TerminalCause::ConnectionClosed)]
#[case::idle(TerminalCause::IdleTimeout)]
#[case::heartbeat(TerminalCause::HeartbeatTimeout)]
#[test]
fn split_keeps_first_terminal_cause(#[case] first: TerminalCause) {
    let shared = SplitShared::new(false, false);
    shared.terminate(first);

    for later in [
        TerminalCause::ConnectionClosed,
        TerminalCause::IdleTimeout,
        TerminalCause::HeartbeatTimeout,
    ] {
        shared.terminate(later);
    }

    assert_eq!(shared.terminal_cause(), Some(first));
}

fn inspect_before_terminal_notification(
    shared: Arc<SplitShared>,
    cause: TerminalCause,
    inspect: impl FnOnce() -> bool + Send,
) -> bool {
    // Hold the watch lock so terminate can publish its atomic state but
    // cannot notify. Readers and writers must already see the typed cause.
    let notification = shared.terminal_tx.borrow();
    std::thread::scope(|scope| {
        scope.spawn(|| shared.terminate(cause));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared.is_open() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let closed = !shared.is_open();
        let (tx, rx) = std::sync::mpsc::channel();
        let later = shared.clone();
        let observer = scope.spawn(move || {
            // A second termination races before the first notification.
            later.terminate(TerminalCause::ConnectionClosed);
            tx.send(inspect()).unwrap();
        });
        // The timeout bounds a broken implementation's lock wait; the
        // publication window itself is fixed by the held watch guard.
        let result = rx.recv_timeout(Duration::from_secs(5));
        drop(notification);
        observer.join().unwrap();
        assert!(closed, "termination did not publish its state");
        result.expect("observing termination must not wait for notification")
    })
}

#[rstest::rstest]
#[case::idle(TerminalCause::IdleTimeout)]
#[case::heartbeat(TerminalCause::HeartbeatTimeout)]
#[tokio::test]
async fn split_reports_terminal_cause_before_notification(#[case] cause: TerminalCause) {
    let (io, _peer) = tokio::io::duplex(64);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, writer) = WebSocketStream::client(io, config).split();
    let shared = reader.shared.clone();
    let expected = cause.error().to_string();

    let observed = inspect_before_terminal_notification(shared, cause, move || {
        matches!(reader.take_terminal(), Some(Some(Err(error))) if error.to_string() == expected)
            && writer.core.current_error().to_string() == expected
            && writer
                .core
                .preferred_write_error(Error::ConnectionClosed)
                .to_string()
                == expected
    });

    assert!(observed);
}

#[cfg(feature = "permessage-deflate")]
#[rstest::rstest]
#[case::idle(TerminalCause::IdleTimeout)]
#[case::heartbeat(TerminalCause::HeartbeatTimeout)]
#[tokio::test]
async fn compressed_split_reports_terminal_cause_before_notification(#[case] cause: TerminalCause) {
    let (io, _peer) = tokio::io::duplex(64);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, writer) =
        CompressedWebSocketStream::client(io, config, crate::DeflateConfig::default()).split();
    let shared = reader.shared.clone();
    let expected = cause.error().to_string();

    let observed = inspect_before_terminal_notification(shared, cause, move || {
        matches!(reader.take_terminal(), Some(Some(Err(error))) if error.to_string() == expected)
            && writer.core.current_error().to_string() == expected
            && writer
                .core
                .preferred_write_error(Error::ConnectionClosed)
                .to_string()
                == expected
    });

    assert!(observed);
}

macro_rules! exhausted_budget_terminal_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let (io, _peer) = tokio::io::duplex(128);
            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
            let (mut reader, _writer) = ($make)(io, config).split();
            reader
                .pending_messages
                .push(Message::binary(b"accepted".to_vec()));
            reader.shared.terminate(TerminalCause::IdleTimeout);
            while tokio::task::coop::has_budget_remaining() {
                tokio::task::coop::consume_budget().await;
            }

            let result = futures_util::poll!(std::pin::pin!(reader.next()));

            assert!(matches!(result, Poll::Ready(Some(Err(Error::IdleTimeout)))));
        }
    };
}

exhausted_budget_terminal_case!(
    split_terminal_precedes_exhausted_delivery_budget,
    WebSocketStream::client
);
#[cfg(feature = "permessage-deflate")]
exhausted_budget_terminal_case!(
    compressed_split_terminal_precedes_exhausted_delivery_budget,
    |io, cfg| CompressedWebSocketStream::client(io, cfg, crate::DeflateConfig::default())
);
