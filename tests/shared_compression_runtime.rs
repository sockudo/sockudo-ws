#![cfg(feature = "permessage-deflate")]

use bytes::{Bytes, BytesMut};
use rstest::rstest;
use sockudo_ws::deflate::{DeflateDecoder, MAX_WINDOW_BITS};
use sockudo_ws::frame::FrameParser;
use sockudo_ws::{Compression, Config, DeflateConfig, Message};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct RecordingIo(Arc<Mutex<Vec<u8>>>);

#[cfg(feature = "tokio-runtime")]
impl tokio::io::AsyncRead for RecordingIo {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Pending
    }
}

#[cfg(feature = "tokio-runtime")]
impl tokio::io::AsyncWrite for RecordingIo {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[cfg(feature = "compio-runtime")]
impl compio::io::AsyncRead for RecordingIo {
    async fn read<B: compio::buf::IoBufMut>(&mut self, _: B) -> compio::BufResult<usize, B> {
        std::future::pending().await
    }
}

#[cfg(feature = "compio-runtime")]
impl compio::io::AsyncWrite for RecordingIo {
    async fn write<B: compio::buf::IoBuf>(&mut self, buf: B) -> compio::BufResult<usize, B> {
        let len = buf.as_init().len();
        self.0.lock().unwrap().extend_from_slice(buf.as_init());
        compio::BufResult(Ok(len), buf)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "compio-runtime")]
impl compio::io::util::Splittable for RecordingIo {
    type ReadHalf = Self;
    type WriteHalf = Self;

    fn split(self) -> (Self, Self) {
        (self.clone(), self)
    }
}

fn config() -> Config {
    Config::builder()
        .compression(Compression::Shared)
        .auto_ping(false)
        .idle_timeout(0)
        .build()
}

fn payload() -> Bytes {
    Bytes::from(b"independent shared compression history ".repeat(128))
}

fn assert_independent_messages(io: &RecordingIo, client: bool) {
    let bytes = io.0.lock().unwrap();
    let mut wire = BytesMut::from(bytes.as_slice());
    let mut parser = FrameParser::with_compression(65536, client);
    for _ in 0..8 {
        let frame = parser.parse(&mut wire).unwrap().unwrap();
        assert!(frame.header.rsv1);
        // Sharing must never require a previous pool user's dictionary, even
        // when negotiation permits the sender to retain context.
        let decoded = DeflateDecoder::new(MAX_WINDOW_BITS, true)
            .decompress(&frame.payload, 65536)
            .unwrap();
        assert_eq!(decoded, payload());
    }
    assert!(wire.is_empty());
}

#[cfg(feature = "tokio-runtime")]
#[rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
#[tokio::test]
async fn tokio_shared_messages_are_independently_decodable(
    #[case] client: bool,
    #[case] split: bool,
) {
    use futures_util::SinkExt;
    use sockudo_ws::CompressedWebSocketStream;

    let io = RecordingIo::default();
    let mut stream = if client {
        CompressedWebSocketStream::client(io.clone(), config(), DeflateConfig::default())
    } else {
        CompressedWebSocketStream::server(io.clone(), config(), DeflateConfig::default())
    };
    if split {
        let (_reader, mut writer) = stream.split();
        for _ in 0..8 {
            writer.send(Message::binary(payload())).await.unwrap();
        }
    } else {
        for _ in 0..8 {
            stream.send(Message::binary(payload())).await.unwrap();
        }
    }
    assert_independent_messages(&io, client);
}

#[cfg(feature = "compio-runtime")]
#[rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
#[compio::test]
async fn compio_shared_messages_are_independently_decodable(
    #[case] client: bool,
    #[case] split: bool,
) {
    use sockudo_ws::compio::CompioCompressedWebSocketStream;

    let io = RecordingIo::default();
    let mut stream = if client {
        CompioCompressedWebSocketStream::client(io.clone(), config(), DeflateConfig::default())
    } else {
        CompioCompressedWebSocketStream::server(io.clone(), config(), DeflateConfig::default())
    };
    if split {
        let (_reader, mut writer) = stream.split();
        for _ in 0..8 {
            writer.send(Message::binary(payload())).await.unwrap();
        }
    } else {
        for _ in 0..8 {
            stream.send(Message::binary(payload())).await.unwrap();
        }
    }
    assert_independent_messages(&io, client);
}
