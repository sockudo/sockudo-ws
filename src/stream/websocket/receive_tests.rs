use futures_util::StreamExt;

use super::*;
#[cfg(feature = "permessage-deflate")]
use crate::receive_tests::deflate_config;
#[cfg(feature = "test-util")]
use crate::receive_tests::two_messages_before_parse_failure_input;
use crate::receive_tests::{
    batch_input, check_close_is_terminal, check_receive_batches, check_valid_before_parse_failure,
    close_before_parse_failure_input, config, parse_failure_input,
};

#[cfg(feature = "test-util")]
async fn check_ping_flush_does_not_reorder_parse_failure<S>(mut stream: S)
where
    S: futures_core::Stream<Item = crate::Result<Message>> + Unpin,
{
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.as_bytes(), &[b'a'; 128]);

    tokio::time::advance(std::time::Duration::from_secs(1)).await;

    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(second.as_bytes(), &[b'b'; 128]);
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}

#[cfg(feature = "test-util")]
fn ping_config() -> Config {
    Config::builder()
        .auto_ping(true)
        .ping_interval(1)
        .pong_timeout(10)
        .idle_timeout(0)
        .build()
}

#[tokio::test]
async fn unified_batches_preserve_owned_messages_and_controls() {
    let (io, writes, expected) = batch_input(false);
    let ws = WebSocketStream::client(io, config());
    check_receive_batches!(ws, writes, expected, Vec::new());
}

#[tokio::test]
async fn split_batches_preserve_pending_messages_and_controls() {
    let (io, writes, expected) = batch_input(false);
    let mut ws = WebSocketStream::client(io, config());
    let first = ws.next().await.unwrap().unwrap();
    let (reader, _writer) = ws.split();
    check_receive_batches!(reader, writes, expected, vec![first]);
}

#[tokio::test]
async fn unified_delivers_valid_message_before_parse_failure() {
    check_valid_before_parse_failure!(WebSocketStream::client(
        parse_failure_input(false),
        config()
    ));
}

#[tokio::test]
async fn unified_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(false);
    check_close_is_terminal!(WebSocketStream::client(io, config()));
}

#[cfg(feature = "test-util")]
#[tokio::test(start_paused = true)]
async fn unified_ping_flush_preserves_messages_before_parse_failure() {
    let (io, _writes) = two_messages_before_parse_failure_input(false);
    check_ping_flush_does_not_reorder_parse_failure(WebSocketStream::client(io, ping_config()))
        .await;
}

#[tokio::test]
async fn split_delivers_valid_message_before_parse_failure() {
    let (reader, _writer) = WebSocketStream::client(parse_failure_input(false), config()).split();
    check_valid_before_parse_failure!(reader);
}

#[tokio::test]
async fn split_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(false);
    let (reader, _writer) = WebSocketStream::client(io, config()).split();
    check_close_is_terminal!(reader);
}

#[tokio::test]
async fn split_handshake_leftover_delivers_valid_message_before_parse_failure() {
    let (io, _peer) = tokio::io::duplex(1024);
    let ws = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Client,
        config(),
        Some(Bytes::from_static(b"\x82\x01x\x83\x00")),
    );
    let (reader, _writer) = ws.split();
    check_valid_before_parse_failure!(reader, Message::binary(Bytes::from_static(b"x")));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_unified_batches_preserve_owned_messages_and_controls() {
    let (io, writes, expected) = batch_input(true);
    let ws = CompressedWebSocketStream::client(io, config(), deflate_config());
    check_receive_batches!(ws, writes, expected, Vec::new());
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_split_batches_preserve_pending_messages_and_controls() {
    let (io, writes, expected) = batch_input(true);
    let mut ws = CompressedWebSocketStream::client(io, config(), deflate_config());
    let first = ws.next().await.unwrap().unwrap();
    let (reader, _writer) = ws.split();
    check_receive_batches!(reader, writes, expected, vec![first]);
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_unified_delivers_valid_message_before_parse_failure() {
    check_valid_before_parse_failure!(CompressedWebSocketStream::client(
        parse_failure_input(true),
        config(),
        deflate_config()
    ));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_unified_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(true);
    check_close_is_terminal!(CompressedWebSocketStream::client(
        io,
        config(),
        deflate_config()
    ));
}

#[cfg(all(feature = "permessage-deflate", feature = "test-util"))]
#[tokio::test(start_paused = true)]
async fn compressed_unified_ping_flush_preserves_messages_before_parse_failure() {
    let (io, _writes) = two_messages_before_parse_failure_input(true);
    check_ping_flush_does_not_reorder_parse_failure(CompressedWebSocketStream::client(
        io,
        ping_config(),
        deflate_config(),
    ))
    .await;
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_split_delivers_valid_message_before_parse_failure() {
    let (reader, _writer) =
        CompressedWebSocketStream::client(parse_failure_input(true), config(), deflate_config())
            .split();
    check_valid_before_parse_failure!(reader);
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_split_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(true);
    let (reader, _writer) =
        CompressedWebSocketStream::client(io, config(), deflate_config()).split();
    check_close_is_terminal!(reader);
}

// Each iteration drops the delivered payload before reading again, so the
// original allocation is eligible for reuse. Ownership retention is exercised
// separately by the batch tests above.
macro_rules! check_receive_window_reuse {
    ($stream:expr, $peer:ident) => {{
        use tokio::io::AsyncWriteExt;
        let mut stream = $stream;
        let mut first = None;
        for _ in 0..32 {
            $peer.write_all(b"\x82\x01x").await.unwrap();
            let message = stream.next().await.unwrap().unwrap();
            assert_eq!(message.as_bytes(), b"x");
            let pointer = message.as_bytes().as_ptr();
            assert_eq!(*first.get_or_insert(pointer), pointer);
            drop(message);
        }
    }};
}

#[tokio::test]
async fn unified_reuses_unretained_receive_window() {
    let (io, mut peer) = tokio::io::duplex(1024);
    check_receive_window_reuse!(WebSocketStream::client(io, config()), peer);
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_unified_reuses_unretained_receive_window() {
    let (io, mut peer) = tokio::io::duplex(1024);
    check_receive_window_reuse!(
        CompressedWebSocketStream::client(io, config(), deflate_config()),
        peer
    );
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_split_reuses_unretained_receive_window() {
    let (io, mut peer) = tokio::io::duplex(1024);
    let (reader, _writer) =
        CompressedWebSocketStream::client(io, config(), deflate_config()).split();
    check_receive_window_reuse!(reader, peer);
}

#[derive(Default)]
struct WakeCount(std::sync::atomic::AtomicUsize);

impl std::task::Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn terminal_registration_after_close_is_ready() {
    let shared = SplitShared::new(false, &config());
    shared.terminate(TerminalCause::IdleTimeout);
    let waker = std::task::Waker::from(Arc::new(WakeCount::default()));
    assert!(
        shared
            .poll_terminal(&mut Context::from_waker(&waker))
            .is_ready()
    );
    assert!(matches!(
        *shared.terminal_tx.borrow(),
        Some(TerminalCause::IdleTimeout)
    ));
}

#[tokio::test]
async fn cancelled_next_reregisters_for_the_new_task() {
    use std::future::Future;
    let (io, _peer) = tokio::io::duplex(1024);
    let (mut reader, _writer) = WebSocketStream::client(io, config()).split();
    let old_task = Arc::new(WakeCount::default());
    let new_task = Arc::new(WakeCount::default());
    for task in [&old_task, &new_task] {
        let waker = std::task::Waker::from(task.clone());
        let mut next = Box::pin(reader.next());
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        // Dropping next cancels this wait without unregistering the reader.
    }
    reader.shared.terminate(TerminalCause::IdleTimeout);
    assert_eq!(old_task.0.load(Ordering::Relaxed), 0);
    assert_eq!(new_task.0.load(Ordering::Relaxed), 1);
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
}

#[tokio::test]
async fn dropping_reader_releases_persistent_terminal_registration() {
    use std::future::Future;
    let (io, _peer) = tokio::io::duplex(1024);
    let (mut reader, _writer) = WebSocketStream::client(io, config()).split();
    let shared = reader.shared.clone();
    let waker = std::task::Waker::from(Arc::new(WakeCount::default()));
    {
        let mut next = Box::pin(reader.next());
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    assert!(shared.reader_waker.lock().unwrap().is_some());
    drop(reader);
    assert!(shared.reader_waker.lock().unwrap().is_none());
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn split_preserves_parsed_prefix_and_error_after_terminal_publication() {
    let (io, _writes) = two_messages_before_parse_failure_input(false);
    let (mut reader, _writer) = WebSocketStream::client(io, config()).split();
    assert_eq!(
        reader.next().await.unwrap().unwrap().as_bytes(),
        &[b'a'; 128]
    );
    // An already discovered parse failure drains its successful prefix before
    // reporting that failure, even if the writer publishes a timeout meanwhile.
    reader.shared.terminate(TerminalCause::IdleTimeout);
    assert_eq!(
        reader.next().await.unwrap().unwrap().as_bytes(),
        &[b'b'; 128]
    );
    assert!(matches!(
        reader.next().await,
        Some(Err(Error::InvalidFrame(_)))
    ));
    assert!(reader.next().await.is_none());
}
