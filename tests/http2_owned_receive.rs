#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use rstest::rstest;
use sockudo_ws::frame::{OpCode, encode_frame};
use sockudo_ws::{Config, Http2Receive, Message, WebSocketStream};
use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct TrackedChunk {
    bytes: Vec<u8>,
    drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl AsRef<[u8]> for TrackedChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for TrackedChunk {
    fn drop(&mut self) {
        self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

struct OwnedTransport(VecDeque<Bytes>);

impl Http2Receive for OwnedTransport {
    fn poll_recv_chunk(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<Bytes>> {
        self.0
            .pop_front()
            .map_or(Poll::Pending, |chunk| Poll::Ready(Ok(chunk)))
    }
}
impl AsyncRead for OwnedTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        panic!("owned receive must not copy through AsyncRead")
    }
}
impl AsyncWrite for OwnedTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn config() -> Config {
    Config {
        auto_ping: false,
        idle_timeout: 0,
        ..Config::default()
    }
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn complete_unmasked_payload_keeps_transport_allocation(#[case] split: bool) {
    let mut frame = BytesMut::new();
    encode_frame(&mut frame, OpCode::Binary, &[42; 128], true, None);
    let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let chunk = Bytes::from_owner(TrackedChunk {
        bytes: frame.to_vec(),
        drops: drops.clone(),
    });
    let payload_ptr = chunk[4..].as_ptr();
    let stream = OwnedTransport(VecDeque::from([chunk]));
    let mut ws = WebSocketStream::client(stream, config()).with_http2_receive_chunks();
    let message = if split {
        let (mut reader, _writer) = ws.split();
        reader.next().await.unwrap().unwrap()
    } else {
        ws.next().await.unwrap().unwrap()
    };
    assert_eq!(message.as_bytes(), &[42; 128]);
    assert_eq!(message.as_bytes().as_ptr(), payload_ptr);
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(message);
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
#[tokio::test]
async fn owned_receive_preserves_frames_at_every_chunk_boundary(
    #[case] split: bool,
    #[case] masked: bool,
) {
    let payload = "行情🙂".repeat(30);
    let mut wire = BytesMut::new();
    encode_frame(
        &mut wire,
        OpCode::Text,
        payload.as_bytes(),
        true,
        masked.then_some([1, 2, 3, 4]),
    );
    encode_frame(
        &mut wire,
        OpCode::Binary,
        b"next",
        true,
        masked.then_some([4, 3, 2, 1]),
    );
    let wire = wire.freeze();
    for boundary in 1..wire.len() {
        let transport = OwnedTransport(VecDeque::from([
            wire.slice(..boundary),
            wire.slice(boundary..),
        ]));
        let mut ws = if masked {
            WebSocketStream::server(transport, config())
        } else {
            WebSocketStream::client(transport, config())
        }
        .with_http2_receive_chunks();
        let (first, second) = if split {
            let (mut reader, _writer) = ws.split();
            (
                reader.next().await.unwrap().unwrap(),
                reader.next().await.unwrap().unwrap(),
            )
        } else {
            (
                ws.next().await.unwrap().unwrap(),
                ws.next().await.unwrap().unwrap(),
            )
        };
        assert_eq!(first.as_bytes(), payload.as_bytes(), "boundary {boundary}");
        assert_eq!(second.as_bytes(), b"next");
    }
}

#[cfg(feature = "permessage-deflate")]
#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn compressed_owned_receive_preserves_context_across_chunks(#[case] split: bool) {
    use sockudo_ws::{CompressedWebSocketStream, protocol::CompressedProtocol};
    let deflate = sockudo_ws::deflate::DeflateConfig::default();
    let mut encoder = CompressedProtocol::server(1024 * 1024, 1024 * 1024, deflate.clone());
    let expected = Message::Text("行情🙂 repeated context ".repeat(200).into());
    let mut wire = BytesMut::new();
    encoder.encode_message(&expected, &mut wire).unwrap();
    encoder.encode_message(&expected, &mut wire).unwrap();
    let wire = wire.freeze();
    let chunks = (0..wire.len()).map(|i| wire.slice(i..i + 1)).collect();
    let mut ws = CompressedWebSocketStream::client(OwnedTransport(chunks), config(), deflate)
        .with_http2_receive_chunks();
    let messages = if split {
        let (mut reader, _writer) = ws.split();
        [
            reader.next().await.unwrap().unwrap(),
            reader.next().await.unwrap().unwrap(),
        ]
    } else {
        [
            ws.next().await.unwrap().unwrap(),
            ws.next().await.unwrap().unwrap(),
        ]
    };
    for message in messages {
        assert_eq!(message.as_bytes(), expected.as_bytes());
    }
}

#[rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn completing_a_partial_frame_preserves_the_following_owned_frame(#[case] split: bool) {
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Binary, &[1; 128], true, None);
    let first_len = wire.len();
    encode_frame(&mut wire, OpCode::Binary, &[2; 128], true, None);
    let wire = wire.freeze();
    let prefix = wire.slice(..5);
    let rest = wire.slice(5..);
    let expected_ptr = rest[first_len - 5 + 4..].as_ptr();
    let mut ws = WebSocketStream::client(OwnedTransport(VecDeque::from([prefix, rest])), config())
        .with_http2_receive_chunks();
    let (first, second) = if split {
        let (mut reader, _writer) = ws.split();
        (
            reader.next().await.unwrap().unwrap(),
            reader.next().await.unwrap().unwrap(),
        )
    } else {
        (
            ws.next().await.unwrap().unwrap(),
            ws.next().await.unwrap().unwrap(),
        )
    };
    assert_eq!(first.as_bytes(), &[1; 128]);
    assert_eq!(second.as_bytes(), &[2; 128]);
    assert_eq!(second.as_bytes().as_ptr(), expected_ptr);
}

#[rstest]
#[case(false, b"\x83\x00")]
#[case(true, b"\x83\x00")]
#[case(false, b"\x81\x03\xff")]
#[case(true, b"\x81\x03\xff")]
#[tokio::test]
async fn owned_receive_delivers_accepted_prefix_before_error(
    #[case] split: bool,
    #[case] invalid: &[u8],
) {
    let mut bytes = BytesMut::from(&b"\x82\x01A"[..]);
    bytes.extend_from_slice(invalid);
    let mut ws =
        WebSocketStream::client(OwnedTransport(VecDeque::from([bytes.freeze()])), config())
            .with_http2_receive_chunks();
    let (first, error) = if split {
        let (mut reader, _writer) = ws.split();
        (
            reader.next().await.unwrap().unwrap(),
            reader.next().await.unwrap().unwrap_err(),
        )
    } else {
        (
            ws.next().await.unwrap().unwrap(),
            ws.next().await.unwrap().unwrap_err(),
        )
    };
    assert_eq!(first.as_bytes(), b"A");
    if invalid[0] == 0x81 {
        assert!(matches!(error, sockudo_ws::Error::InvalidUtf8));
    } else {
        assert!(matches!(
            error,
            sockudo_ws::Error::InvalidFrame("invalid opcode")
        ));
    }
}
