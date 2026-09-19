#![cfg(feature = "compio-runtime")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::util::Splittable;
use compio::io::{AsyncRead, AsyncWrite};
use futures_channel::oneshot;
use futures_util::poll;
use sockudo_ws::protocol::{Message, Protocol, Role};
use sockudo_ws::{CompioWebSocketStream, Config, Error};

#[derive(Default)]
struct WriteState {
    bytes: RefCell<Vec<u8>>,
    cancelled: Cell<bool>,
    writer_dropped: Cell<bool>,
    reader_dropped: Cell<bool>,
    write_limit: Cell<usize>,
    block_flush: Cell<bool>,
    write_waker: RefCell<Option<Waker>>,
    input: RefCell<VecDeque<io::Result<Vec<u8>>>>,
    read_waker: RefCell<Option<Waker>>,
}

impl WriteState {
    fn push_input(&self, input: io::Result<Vec<u8>>) {
        self.input.borrow_mut().push_back(input);
        if let Some(waker) = self.read_waker.borrow_mut().take() {
            waker.wake();
        }
    }

    fn release_write(&self) {
        self.write_limit.set(usize::MAX);
        if let Some(waker) = self.write_waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

struct PendingWrite {
    state: Rc<WriteState>,
    completed: bool,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        if !self.completed {
            self.state.cancelled.set(true);
        }
    }
}

struct PartialWriter {
    state: Rc<WriteState>,
    blocked: Option<oneshot::Sender<()>>,
}

impl AsyncWrite for PartialWriter {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let mut pending = PendingWrite {
            state: self.state.clone(),
            completed: false,
        };
        // The owned buffer stays in this future until completion or cancellation.
        let result = poll_fn(|cx| {
            let mut bytes = self.state.bytes.borrow_mut();
            let available = self.state.write_limit.get() - bytes.len();
            if available != 0 {
                let count = buf.as_init().len().min(available);
                bytes.extend_from_slice(&buf.as_init()[..count]);
                return Poll::Ready(Ok(count));
            }
            if let Some(blocked) = self.blocked.take() {
                let _ = blocked.send(());
            }
            self.state.write_waker.replace(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
        pending.completed = true;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        if self.state.block_flush.get() {
            let _pending = PendingWrite {
                state: self.state.clone(),
                completed: false,
            };
            self.blocked.take().unwrap().send(()).unwrap();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn shutdown(&mut self) -> io::Result<()> {
        std::future::pending().await
    }
}

impl Drop for PartialWriter {
    fn drop(&mut self) {
        self.state.writer_dropped.set(true);
    }
}

struct PendingReader(Rc<WriteState>);

impl AsyncRead for PendingReader {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let input = poll_fn(|cx| {
            if let Some(input) = self.0.input.borrow_mut().pop_front() {
                return Poll::Ready(input);
            }
            self.0.read_waker.replace(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
        match input {
            Ok(input) => {
                assert!(input.len() <= buf.buf_capacity());
                io::Cursor::new(input).read(buf).await
            }
            Err(error) => BufResult(Err(error), buf),
        }
    }
}

impl Drop for PendingReader {
    fn drop(&mut self) {
        self.0.reader_dropped.set(true);
    }
}

struct TestIo<R>(R, PartialWriter);

impl<R: AsyncRead> AsyncRead for TestIo<R> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.read(buf).await
    }
}

impl<R> AsyncWrite for TestIo<R> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.1.write(buf).await
    }
    async fn flush(&mut self) -> io::Result<()> {
        self.1.flush().await
    }
    async fn shutdown(&mut self) -> io::Result<()> {
        self.1.shutdown().await
    }
}

impl<R> Splittable for TestIo<R> {
    type ReadHalf = R;
    type WriteHalf = PartialWriter;
    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (self.0, self.1)
    }
}

fn connection() -> (TestIo<PendingReader>, Rc<WriteState>, oneshot::Receiver<()>) {
    let state = Rc::new(WriteState::default());
    state.write_limit.set(3);
    let (blocked, entered) = oneshot::channel();
    (
        TestIo(
            PendingReader(state.clone()),
            PartialWriter {
                state: state.clone(),
                blocked: Some(blocked),
            },
        ),
        state,
        entered,
    )
}

#[compio::test]
async fn idle_expiry_cancels_partial_write_before_notifying_sender() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send_state = state.clone();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial message").await;
        assert!(send_state.cancelled.get());
        assert!(send_state.writer_dropped.get());
        (writer, result)
    });
    entered.await.unwrap();
    let (_writer, result) = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("idle timeout must interrupt the pending write")
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(
        state.bytes.borrow().len(),
        3,
        "no Close may follow a partial frame"
    );
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
}

#[compio::test]
async fn queued_send_observes_the_partial_writes_idle_timeout() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    assert!(poll!(std::pin::pin!(writer.send_text("active"))).is_pending());
    entered.await.unwrap();
    let result = compio::time::timeout(Duration::from_secs(2), writer.send_text("queued"))
        .await
        .expect("queued caller must receive the active write's timeout");
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(state.cancelled.get());
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_partial_write_observes_idle_expiry() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default(),
    )
    .split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("compressed payload".repeat(100)).await;
        (writer, result)
    });
    entered.await.unwrap();
    let (_writer, result) = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("compressed writes must enforce idle timeout")
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(state.cancelled.get());
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn read_failure_returns_without_waiting_for_driver() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    state.push_input(Err(io::Error::from_raw_os_error(13)));

    assert!(matches!(
        poll!(std::pin::pin!(reader.next())),
        Poll::Ready(Some(Err(Error::Io(error)))) if error.raw_os_error() == Some(13)
    ));
    assert!(reader.is_closed());
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("a read failure must cancel the blocked writer")
        .unwrap();
    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert!(state.cancelled.get());
    assert!(state.writer_dropped.get());
    assert!(!state.reader_dropped.get());
    assert!(reader.next().await.is_none());
}

#[compio::test]
async fn writer_drop_precedes_sender_and_pending_reader_notifications() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let read_state = state.clone();
    let read = compio::runtime::spawn(async move {
        let result = reader.next().await;
        assert!(read_state.cancelled.get());
        assert!(read_state.writer_dropped.get());
        (reader, result)
    });
    let send_state = state.clone();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        assert!(send_state.cancelled.get());
        assert!(send_state.writer_dropped.get());
        (writer, result)
    });
    entered.await.unwrap();
    let (read, send) = compio::time::timeout(Duration::from_secs(2), async {
        futures_util::join!(read, send)
    })
    .await
    .unwrap();
    let (_reader, received) = read.unwrap();
    let (_writer, sent) = send.unwrap();
    assert!(matches!(received, Some(Err(Error::IdleTimeout))));
    assert!(matches!(sent, Err(Error::IdleTimeout)));
    assert!(!state.reader_dropped.get());
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn parser_failure_cancels_a_blocked_writer() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    state.push_input(Ok(vec![0x83, 0]));

    assert!(matches!(
        poll!(std::pin::pin!(reader.next())),
        Poll::Ready(Some(Err(Error::InvalidFrame("invalid opcode"))))
    ));
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("a parse failure must cancel the blocked writer")
        .unwrap();
    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert!(state.cancelled.get());
    assert!(state.writer_dropped.get());
    assert!(!state.reader_dropped.get());
    assert!(reader.next().await.is_none());
}

#[compio::test]
async fn eof_from_a_borrowed_reader_cancels_a_blocked_writer() {
    let (TestIo(mut transport_reader, transport_writer), state, entered) = connection();
    let io = TestIo(&mut transport_reader, transport_writer);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    state.push_input(Ok(Vec::new()));

    assert!(matches!(
        poll!(std::pin::pin!(reader.next())),
        Poll::Ready(None)
    ));
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("EOF must cancel the blocked writer")
        .unwrap();
    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert!(state.cancelled.get());
    assert!(state.writer_dropped.get());
    assert!(!state.reader_dropped.get());
    assert!(reader.next().await.is_none());
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_parser_failure_cancels_a_blocked_writer() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, mut writer) = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default(),
    )
    .split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    state.push_input(Ok(vec![0x83, 0]));

    assert!(matches!(
        poll!(std::pin::pin!(reader.next())),
        Poll::Ready(Some(Err(Error::InvalidFrame("invalid opcode"))))
    ));
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("a compressed parse failure must cancel the blocked writer")
        .unwrap();
    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert!(state.cancelled.get());
    assert!(state.writer_dropped.get());
    assert!(!state.reader_dropped.get());
    assert!(reader.next().await.is_none());
}

#[compio::test]
async fn blocked_flush_enforces_idle_timeout() {
    let (io, state, entered) = connection();
    state.block_flush.set(true);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let flush = compio::runtime::spawn(async move {
        let result = writer.flush().await;
        (writer, result)
    });
    entered.await.unwrap();
    let (_writer, result) = compio::time::timeout(Duration::from_secs(2), flush)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(state.cancelled.get());
    assert!(state.writer_dropped.get());
    assert!(state.bytes.borrow().is_empty());
}

#[compio::test]
async fn inbound_data_defers_idle_expiry_during_a_partial_write() {
    let (io, state, entered) = connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    compio::time::sleep(Duration::from_millis(600)).await;
    state.push_input(Ok(vec![0x82, 1, b'x']));
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    compio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !reader.is_closed(),
        "activity must invalidate the old idle deadline"
    );
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn a_due_ping_does_not_hide_a_blocked_writes_idle_deadline() {
    let (io, state, entered) = connection();
    let config = Config::builder().ping_interval(1).idle_timeout(2).build();
    let (_reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    let (_writer, result) = compio::time::timeout(Duration::from_secs(3), send)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(
        state.bytes.borrow().len(),
        3,
        "a Ping cannot interrupt the frame"
    );
}

#[compio::test]
async fn inbound_data_does_not_extend_an_outstanding_pong_deadline() {
    let (io, state, entered) = connection();
    state.write_limit.set(17); // One masked Ping, then three bytes of the data frame.
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(10)
        .build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        decode_outbound(&state).as_slice(),
        [Message::Ping(_)]
    ));
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial").await;
        (writer, result)
    });
    entered.await.unwrap();
    state.push_input(Ok(vec![0x82, 1, b'x']));
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    let (_writer, result) = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::HeartbeatTimeout)));
    assert_eq!(state.bytes.borrow().len(), 17);
}

#[compio::test]
async fn timely_pong_behind_peer_pings_preserves_the_partial_frame() {
    let (io, state, entered) = connection();
    state.write_limit.set(17);
    let config = Config::builder()
        .ping_interval(2)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    compio::time::sleep(Duration::from_millis(2100)).await;
    let messages = decode_outbound(&state);
    let [Message::Ping(payload)] = messages.as_slice() else {
        panic!("expected the scheduled Ping");
    };
    let mut incoming = BytesMut::new();
    let mut peer = Protocol::new(Role::Server, 65536, 65536);
    for message in [
        Message::Ping(Bytes::from_static(b"old")),
        Message::Ping(Bytes::from_static(b"new")),
        Message::Pong(payload.clone()),
    ] {
        peer.encode_message(&message, &mut incoming).unwrap();
    }
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("complete after Pong").await;
        (writer, result)
    });
    entered.await.unwrap();
    state.push_input(Ok(incoming.to_vec()));
    for _ in 0..3 {
        reader.next().await.unwrap().unwrap();
    }
    // Queue the timely receipt, then keep the driver unpolled past its deadline.
    std::thread::sleep(Duration::from_millis(1100));
    state.release_write();
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
    assert!(!reader.is_closed());
    let messages = decode_outbound(&state);
    assert!(
        matches!(messages.as_slice(), [Message::Ping(_), Message::Text(text), Message::Pong(pong)]
        if text == "complete after Pong" && pong.as_ref() == b"new")
    );
}

#[compio::test]
async fn deferred_ping_starts_its_pong_clock_only_after_flush() {
    let (io, state, entered) = connection();
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("complete before Ping").await;
        (writer, result)
    });
    entered.await.unwrap();
    compio::time::sleep(Duration::from_millis(2100)).await;
    assert!(!reader.is_closed());
    assert_eq!(state.bytes.borrow().len(), 3);
    state.release_write();
    let (_writer, result) = send.await.unwrap();
    result.unwrap();
    let messages = decode_outbound(&state);
    assert!(
        matches!(messages.as_slice(), [Message::Text(text), Message::Ping(_)] if text == "complete before Ping")
    );
    compio::time::sleep(Duration::from_millis(600)).await;
    assert!(!reader.is_closed());
    let result = compio::time::timeout(Duration::from_secs(1), reader.next())
        .await
        .unwrap();
    assert!(matches!(result, Some(Err(Error::HeartbeatTimeout))));
}

#[compio::test]
async fn a_partially_written_ping_does_not_start_its_pong_timeout() {
    let (io, state, entered) = connection();
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut reader, _writer) = CompioWebSocketStream::client(io, config).split();
    compio::time::timeout(Duration::from_secs(2), entered)
        .await
        .unwrap()
        .unwrap();
    compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!reader.is_closed());
    assert_eq!(state.bytes.borrow().len(), 3);
    state.release_write();
    compio::time::sleep(Duration::from_millis(600)).await;
    assert!(!reader.is_closed());
    assert!(matches!(
        decode_outbound(&state).as_slice(),
        [Message::Ping(_)]
    ));
    let result = compio::time::timeout(Duration::from_secs(1), reader.next())
        .await
        .unwrap();
    assert!(matches!(result, Some(Err(Error::HeartbeatTimeout))));
}

fn decode_outbound(state: &WriteState) -> Vec<Message> {
    Protocol::new(Role::Server, 65536, 65536)
        .process(&mut BytesMut::from(state.bytes.borrow().as_slice()))
        .unwrap()
}

#[compio::test]
async fn tcp_backpressure_reports_timeout_with_both_handles_retained() {
    use compio::net::{TcpListener, TcpStream};
    use socket2::SockRef;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (peer, _) = listener.accept().await.unwrap();
    SockRef::from(&stream).set_send_buffer_size(8192).unwrap();
    SockRef::from(&peer).set_recv_buffer_size(8192).unwrap();
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(1)
        // Exercise TCP backpressure rather than the encoded-buffer limit.
        .max_backpressure(8 * 1024 * 1024)
        .max_frame_size(8 * 1024 * 1024)
        .max_message_size(8 * 1024 * 1024)
        .build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(stream, config).split();
    let payload_len = 4 * 1024 * 1024;
    let mut send = compio::runtime::spawn(async move {
        let result = writer
            .send_binary(Bytes::from(vec![b'x'; payload_len]))
            .await;
        (writer, result)
    });
    compio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        poll!(&mut send).is_pending(),
        "the peer must exert backpressure"
    );
    let (_writer, result) = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
}
