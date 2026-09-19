use std::collections::VecDeque;
use std::io;

use bytes::{Bytes, BytesMut};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::Config;
use crate::protocol::{Message, Protocol, Role};

pub(crate) struct BatchIo {
    batches: VecDeque<Bytes>,
    writes: UnboundedSender<Bytes>,
    fail_writes: bool,
}

impl BatchIo {
    fn new(batches: Vec<Bytes>) -> (Self, UnboundedReceiver<Bytes>) {
        let (writes, received) = unbounded_channel();
        (
            Self {
                batches: batches.into(),
                writes,
                fail_writes: false,
            },
            received,
        )
    }

    #[cfg(feature = "compio-runtime")]
    fn with_write_failure(batches: Vec<Bytes>) -> Self {
        let (writes, _received) = unbounded_channel();
        Self {
            batches: batches.into(),
            writes,
            fail_writes: true,
        }
    }
}

#[cfg(feature = "tokio-runtime")]
impl tokio::io::AsyncRead for BatchIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        if let Some(batch) = self.batches.pop_front() {
            assert!(
                batch.len() <= buf.remaining(),
                "test batch must fit one read"
            );
            buf.put_slice(&batch);
        }
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "tokio-runtime")]
impl tokio::io::AsyncWrite for BatchIo {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        if self.fail_writes {
            return std::task::Poll::Ready(Err(io::Error::other("intentional test write failure")));
        }
        self.writes.send(Bytes::copy_from_slice(bytes)).unwrap();
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "compio-runtime")]
impl compio::io::AsyncRead for BatchIo {
    async fn read<B: compio::buf::IoBufMut>(
        &mut self,
        mut buf: B,
    ) -> compio::buf::BufResult<usize, B> {
        let batch = self.batches.pop_front().unwrap_or_default();
        assert!(
            batch.len() <= buf.buf_capacity(),
            "test batch must fit one read"
        );
        // Delegate buffer initialization to the runtime's slice reader.
        compio::io::AsyncRead::read(&mut batch.as_ref(), buf).await
    }
}

#[cfg(feature = "compio-runtime")]
impl compio::io::AsyncWrite for BatchIo {
    async fn write<B: compio::buf::IoBuf>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
        if self.fail_writes {
            return compio::buf::BufResult(
                Err(io::Error::other("intentional test write failure")),
                buf,
            );
        }
        let bytes = buf.as_init();
        self.writes.send(Bytes::copy_from_slice(bytes)).unwrap();
        compio::buf::BufResult(Ok(bytes.len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "compio-runtime")]
impl compio::io::util::Splittable for BatchIo {
    type ReadHalf = compio::io::util::split::ReadHalf<Self>;
    type WriteHalf = compio::io::util::split::WriteHalf<Self>;

    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        compio::io::split(self)
    }
}

pub(crate) fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

#[cfg(feature = "permessage-deflate")]
pub(crate) fn deflate_config() -> crate::deflate::DeflateConfig {
    crate::deflate::DeflateConfig {
        server_no_context_takeover: true,
        client_no_context_takeover: true,
        ..Default::default()
    }
}

pub(crate) fn batch_input(compressed: bool) -> (BatchIo, UnboundedReceiver<Bytes>, Vec<Message>) {
    let mut batches = Vec::new();
    let mut expected = Vec::new();
    for batch in 0..3u8 {
        let mut messages = Vec::new();
        for index in 0..16u8 {
            let mut payload = vec![b'a' + index; 128];
            payload[0] = b'0' + batch;
            messages.push(if index % 2 == 0 {
                Message::Binary(payload.into())
            } else {
                Message::Text(payload.into())
            });
            if index == 5 {
                messages.push(Message::Ping(Bytes::from(vec![batch, 0xaa])));
            }
            if index == 10 {
                messages.push(Message::Pong(Bytes::from(vec![batch, 0xbb])));
            }
        }
        if batch == 2 {
            messages.push(Message::Close(None));
        }
        batches.push(encode_batch(&messages, compressed));
        expected.extend(messages);
    }
    let (io, writes) = BatchIo::new(batches);
    (io, writes, expected)
}

pub(crate) fn parse_failure_input(compressed: bool) -> BatchIo {
    let mut wire =
        BytesMut::from(encode_batch(&[Message::binary(vec![b'x'; 128])], compressed).as_ref());
    // Both frames arrive together, so parsing fails after producing partial output.
    wire.extend_from_slice(b"\x83\x00");
    let (io, _writes) = BatchIo::new(vec![wire.freeze()]);
    io
}

pub(crate) fn two_messages_before_parse_failure_input(
    compressed: bool,
) -> (BatchIo, UnboundedReceiver<Bytes>) {
    let messages = [
        Message::binary(vec![b'a'; 128]),
        Message::binary(vec![b'b'; 128]),
    ];
    let mut wire = BytesMut::from(encode_batch(&messages, compressed).as_ref());
    wire.extend_from_slice(b"\x83\x00");
    BatchIo::new(vec![wire.freeze()])
}

pub(crate) fn close_before_parse_failure_input(
    compressed: bool,
) -> (BatchIo, UnboundedReceiver<Bytes>) {
    let mut wire = BytesMut::from(encode_batch(&[Message::Close(None)], compressed).as_ref());
    wire.extend_from_slice(b"\x83\x00");
    BatchIo::new(vec![wire.freeze()])
}

#[cfg(feature = "compio-runtime")]
pub(crate) fn pending_after_failed_control_reply_input(compressed: bool) -> BatchIo {
    let wire = encode_batch(
        &[
            Message::Ping(Bytes::from_static(b"ping")),
            Message::binary(vec![b'x'; 128]),
        ],
        compressed,
    );
    BatchIo::with_write_failure(vec![wire])
}

fn encode_batch(messages: &[Message], compressed: bool) -> Bytes {
    let mut wire = BytesMut::new();
    #[cfg(feature = "permessage-deflate")]
    if compressed {
        let mut protocol =
            crate::protocol::CompressedProtocol::server(8192, 8192, deflate_config());
        for message in messages {
            let start = wire.len();
            protocol.encode_message(message, &mut wire).unwrap();
            if matches!(message, Message::Text(_) | Message::Binary(_)) {
                assert_ne!(wire[start] & 0x40, 0, "exercise actual compressed frames");
            }
        }
        return wire.freeze();
    }
    assert!(!compressed);
    let mut protocol = Protocol::new(Role::Server, 8192, 8192);
    for message in messages {
        protocol.encode_message(message, &mut wire).unwrap();
    }
    wire.freeze()
}

pub(crate) fn assert_messages(actual: &[Message], expected: &[Message]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            std::mem::discriminant(actual),
            std::mem::discriminant(expected)
        );
        assert_eq!(actual.as_bytes(), expected.as_bytes());
    }
}

pub(crate) async fn assert_control_reply(writes: &mut UnboundedReceiver<Bytes>, expected: Message) {
    let mut protocol = Protocol::new(Role::Server, 1024, 1024);
    let mut wire = BytesMut::new();
    loop {
        wire.extend_from_slice(&writes.recv().await.unwrap());
        let replies = protocol.process(&mut wire).unwrap();
        if !replies.is_empty() {
            assert_messages(&replies, &[expected]);
            assert!(wire.is_empty());
            return;
        }
    }
}

macro_rules! check_receive_batches {
    ($stream:expr, $writes:expr, $expected:expr, $received:expr) => {{
        let mut stream = $stream;
        let mut writes = $writes;
        let expected = $expected;
        let mut received = $received;
        for _ in received.len()..expected.len() {
            let message = stream.next().await.unwrap().unwrap();
            // Observe each Pong before a later Close can cancel pending writes.
            let reply = match &message {
                $crate::Message::Ping(payload) => Some($crate::Message::Pong(payload.clone())),
                $crate::Message::Close(_) => Some($crate::Message::Close(None)),
                _ => None,
            };
            if let Some(reply) = reply {
                $crate::receive_tests::assert_control_reply(&mut writes, reply).await;
            }
            received.push(message);
        }
        drop(stream);
        $crate::receive_tests::assert_messages(&received, &expected);
    }};
}

macro_rules! check_valid_before_parse_failure {
    ($stream:expr) => {{
        $crate::receive_tests::check_valid_before_parse_failure!(
            $stream,
            $crate::Message::binary(vec![b'x'; 128])
        );
    }};
    ($stream:expr, $expected:expr) => {{
        let mut stream = $stream;
        let message = stream.next().await.unwrap().unwrap();
        $crate::receive_tests::assert_messages(&[message], &[$expected]);
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
    }};
}

macro_rules! check_close_is_terminal {
    ($stream:expr) => {{
        let mut stream = $stream;
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            $crate::Message::Close(_)
        ));
        assert!(stream.next().await.is_none());
    }};
}

pub(crate) use {check_close_is_terminal, check_receive_batches, check_valid_before_parse_failure};
