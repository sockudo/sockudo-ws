#![cfg(feature = "compio-runtime")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::io;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, util::Splittable};
use futures_channel::oneshot;
use sockudo_ws::protocol::{Message, Protocol, Role};
use sockudo_ws::{CompioWebSocketStream, Config, Error};

#[derive(Default)]
struct WriteState {
    bytes: RefCell<Vec<u8>>,
    cancelled: Cell<bool>,
    writer_dropped: Cell<bool>,
    write_limit: Cell<usize>,
    block_flush: Cell<bool>,
    write_waker: RefCell<Option<Waker>>,
    input: RefCell<VecDeque<io::Result<Vec<u8>>>>,
    read_waker: RefCell<Option<Waker>>,
}

impl WriteState {
    fn push_input(&self, input: Vec<u8>) {
        self.input.borrow_mut().push_back(Ok(input));
        if let Some(waker) = self.read_waker.borrow_mut().take() {
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
        let result = poll_fn(|cx| {
            let mut bytes = self.state.bytes.borrow_mut();
            let write_limit = self.state.write_limit.get();
            if bytes.len() < write_limit {
                let count = buf.as_init().len().min(write_limit - bytes.len());
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
        if !self.state.block_flush.get() {
            return Ok(());
        }
        let mut pending = PendingWrite {
            state: self.state.clone(),
            completed: false,
        };
        let result = poll_fn(|cx| {
            if let Some(blocked) = self.blocked.take() {
                let _ = blocked.send(());
            }
            self.state.write_waker.replace(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
        pending.completed = true;
        result
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PartialWriter {
    fn drop(&mut self) {
        self.state.writer_dropped.set(true);
    }
}

struct PendingReader(Rc<WriteState>);

impl AsyncRead for PendingReader {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        let input = poll_fn(|cx| {
            if let Some(input) = self.0.input.borrow_mut().pop_front() {
                return Poll::Ready(input);
            }
            self.0.read_waker.replace(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
        match input {
            Ok(input) => io::Cursor::new(input).read(buf).await,
            Err(error) => BufResult(Err(error), buf),
        }
    }
}

struct TestIo(PendingReader, PartialWriter);

impl AsyncRead for TestIo {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.read(buf).await
    }
}

impl AsyncWrite for TestIo {
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

impl Splittable for TestIo {
    type ReadHalf = PendingReader;
    type WriteHalf = PartialWriter;

    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (self.0, self.1)
    }
}

fn partial_connection() -> (TestIo, Rc<WriteState>, oneshot::Receiver<()>) {
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

fn blocked_flush_connection() -> (TestIo, Rc<WriteState>, oneshot::Receiver<()>) {
    let state = Rc::new(WriteState::default());
    state.write_limit.set(usize::MAX);
    state.block_flush.set(true);
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

fn immediate_connection() -> (TestIo, Rc<WriteState>) {
    let state = Rc::new(WriteState::default());
    state.write_limit.set(usize::MAX);
    (
        TestIo(
            PendingReader(state.clone()),
            PartialWriter {
                state: state.clone(),
                blocked: None,
            },
        ),
        state,
    )
}

#[compio::test]
async fn idle_timeout_cancels_a_partial_send_before_notifying_handles() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();

    let read_state = state.clone();
    let read = compio::runtime::spawn(async move {
        let result = reader.next().await;
        assert!(read_state.cancelled.get());
        assert!(read_state.writer_dropped.get());
        result
    });
    let send_state = state.clone();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial message").await;
        assert!(send_state.cancelled.get());
        assert!(send_state.writer_dropped.get());
        result
    });

    entered.await.unwrap();
    let (received, sent) = compio::time::timeout(Duration::from_secs(2), async {
        futures_util::join!(read, send)
    })
    .await
    .expect("idle timeout must interrupt the pending write");

    assert!(matches!(received.unwrap(), Some(Err(Error::IdleTimeout))));
    assert!(matches!(sent.unwrap(), Err(Error::IdleTimeout)));
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn parse_error_preserves_an_existing_partial_send() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder().auto_ping(false).close_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();

    let send = compio::runtime::spawn(async move { writer.send_text("partial message").await });
    entered.await.unwrap();
    state.push_input(vec![0x83, 0x00]);

    assert!(reader.next().await.unwrap().is_err());
    compio::time::sleep(Duration::from_millis(50)).await;
    assert!(!state.cancelled.get());
    assert!(!state.writer_dropped.get());

    state.write_limit.set(usize::MAX);
    state.write_waker.borrow_mut().take().unwrap().wake();
    let sent = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("the in-progress write must be allowed to finish")
        .unwrap();
    assert!(sent.is_ok());
}

#[compio::test]
async fn parse_error_bounds_an_existing_partial_send() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder().auto_ping(false).close_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();

    let send = compio::runtime::spawn(async move { writer.send_text("partial message").await });
    entered.await.unwrap();
    state.push_input(vec![0x83, 0x00]);

    assert!(reader.next().await.unwrap().is_err());
    let sent = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("the parse error must bound the in-progress write")
        .unwrap();
    assert!(matches!(sent, Err(Error::ConnectionClosed)));
    assert!(state.cancelled.get());
    assert!(state.writer_dropped.get());
}

#[compio::test]
async fn idle_timeout_cancels_a_blocked_flush() {
    let (io, state, entered) = blocked_flush_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let flush_state = state.clone();
    let flush = compio::runtime::spawn(async move {
        let result = writer.flush().await;
        assert!(flush_state.cancelled.get());
        assert!(flush_state.writer_dropped.get());
        result
    });

    entered.await.unwrap();
    let result = compio::time::timeout(Duration::from_secs(2), flush)
        .await
        .expect("idle timeout must interrupt the pending flush")
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert!(state.bytes.borrow().is_empty());
}

#[compio::test]
async fn inbound_data_defers_idle_expiry_during_a_partial_send() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial message").await;
        (writer, result)
    });
    entered.await.unwrap();

    compio::time::sleep(Duration::from_millis(600)).await;
    let mut wire = bytes::BytesMut::new();
    Protocol::new(Role::Server, 65_536, 65_536)
        .encode_message(&Message::binary(vec![1]), &mut wire)
        .unwrap();
    state.push_input(wire.to_vec());
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), &[1]);

    compio::time::sleep(Duration::from_millis(600)).await;
    assert!(!reader.is_closed());
    let (_writer, result) = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("the refreshed idle deadline must eventually expire")
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn data_does_not_extend_a_blocked_writes_pong_deadline() {
    let (io, state, entered) = partial_connection();
    // One masked Ping is 14 bytes; allow three bytes of the data frame after it.
    state.write_limit.set(17);
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(10)
        .build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();

    compio::time::sleep(Duration::from_millis(1100)).await;
    let outbound = Protocol::new(Role::Server, 65_536, 65_536)
        .process(&mut bytes::BytesMut::from(state.bytes.borrow().as_slice()))
        .unwrap();
    assert!(matches!(outbound.as_slice(), [Message::Ping(_)]));

    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial message").await;
        (writer, result)
    });
    entered.await.unwrap();
    let mut wire = bytes::BytesMut::new();
    Protocol::new(Role::Server, 65_536, 65_536)
        .encode_message(&Message::binary(vec![1]), &mut wire)
        .unwrap();
    state.push_input(wire.to_vec());
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), &[1]);

    let (_writer, result) = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("Pong timeout must interrupt the partial write")
        .unwrap();
    assert!(matches!(result, Err(Error::HeartbeatTimeout)));
    assert_eq!(state.bytes.borrow().len(), 17);
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_partial_send_observes_the_idle_timeout() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_reader, mut writer) = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::DeflateConfig::default(),
    )
    .split();
    let send_state = state.clone();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("compressible payload ".repeat(100)).await;
        assert!(send_state.cancelled.get());
        assert!(send_state.writer_dropped.get());
        result
    });

    entered.await.unwrap();
    let result = compio::time::timeout(Duration::from_secs(2), send)
        .await
        .expect("compressed send must observe the idle timeout")
        .unwrap();
    assert!(matches!(result, Err(Error::IdleTimeout)));
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn peer_close_bounds_an_existing_partial_send() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let send_state = state.clone();
    let send = compio::runtime::spawn(async move {
        let result = writer.send_text("partial message").await;
        assert!(send_state.cancelled.get());
        assert!(send_state.writer_dropped.get());
        result
    });
    entered.await.unwrap();

    let mut wire = bytes::BytesMut::new();
    Protocol::new(Role::Server, 65_536, 65_536)
        .encode_message(&Message::Close(None), &mut wire)
        .unwrap();
    state.push_input(wire.to_vec());
    assert!(reader.next().await.unwrap().unwrap().is_close());

    let result = compio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("peer Close must bound the existing write")
        .unwrap();
    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[derive(Clone)]
struct RecordingIo(Rc<RefCell<Vec<u8>>>);

impl AsyncRead for RecordingIo {
    async fn read<B: IoBufMut>(&mut self, _buf: B) -> BufResult<usize, B> {
        std::future::pending().await
    }
}

impl AsyncWrite for RecordingIo {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.0.borrow_mut().extend_from_slice(buf.as_init());
        BufResult(Ok(buf.as_init().len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Splittable for RecordingIo {
    type ReadHalf = Self;
    type WriteHalf = Self;

    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (self.clone(), self)
    }
}

#[compio::test]
async fn zero_close_timeout_keeps_an_immediate_best_effort_write() {
    let bytes = Rc::new(RefCell::new(Vec::new()));
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (_reader, mut writer) =
        CompioWebSocketStream::client(RecordingIo(bytes.clone()), config).split();

    writer.close(1000, "").await.unwrap();
    assert_eq!(bytes.borrow().first(), Some(&0x88));
}

#[compio::test]
async fn zero_close_timeout_keeps_an_immediate_peer_close_response() {
    let mut wire = bytes::BytesMut::new();
    Protocol::new(Role::Server, 65_536, 65_536)
        .encode_message(&Message::Close(None), &mut wire)
        .unwrap();

    for _ in 0..32 {
        let (io, state) = immediate_connection();
        state.push_input(wire.to_vec());
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .close_timeout(0)
            .build();
        let (mut reader, _writer) = CompioWebSocketStream::client(io, config).split();

        assert!(reader.next().await.unwrap().unwrap().is_close());
        assert!(reader.next().await.is_none());
        assert_eq!(state.bytes.borrow().first(), Some(&0x88));
    }
}

#[compio::test]
async fn zero_close_timeout_cancels_a_partial_close() {
    let (io, state, entered) = partial_connection();
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (_reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    let close_state = state.clone();
    let close = compio::runtime::spawn(async move {
        let result = writer.close(1000, "").await;
        assert!(close_state.cancelled.get());
        assert!(close_state.writer_dropped.get());
        result
    });

    entered.await.unwrap();
    let result = compio::time::timeout(Duration::from_secs(1), close)
        .await
        .expect("zero Close timeout must not wait for the blocked write")
        .unwrap();
    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert_eq!(state.bytes.borrow().len(), 3);
}

#[compio::test]
async fn cancelling_an_enqueued_send_completes_it_without_closing() {
    let bytes = Rc::new(RefCell::new(Vec::new()));
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (_reader, mut writer) =
        CompioWebSocketStream::client(RecordingIo(bytes.clone()), config).split();

    {
        let send = writer.send_text("first");
        futures_util::pin_mut!(send);
        assert!(
            poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx)))
                .await
                .is_pending()
        );
    }
    writer.send_text("second").await.unwrap();

    let mut wire = bytes::BytesMut::from(bytes.borrow().as_slice());
    let messages = Protocol::new(Role::Server, 65_536, 65_536)
        .process(&mut wire)
        .unwrap();
    assert!(
        matches!(messages.as_slice(), [Message::Text(first), Message::Text(second)] if first == "first" && second == "second")
    );
    assert!(!writer.is_closed());
}

#[compio::test]
async fn cancelling_an_enqueued_close_keeps_the_connection_closing() {
    let bytes = Rc::new(RefCell::new(Vec::new()));
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (_reader, mut writer) =
        CompioWebSocketStream::client(RecordingIo(bytes.clone()), config).split();

    {
        let close = writer.close(1000, "");
        futures_util::pin_mut!(close);
        assert!(
            poll_fn(|cx| Poll::Ready(close.as_mut().poll(cx)))
                .await
                .is_pending()
        );
    }

    assert!(matches!(
        writer.send_text("later").await,
        Err(Error::ConnectionClosed)
    ));
    assert_eq!(bytes.borrow().first(), Some(&0x88));
}
