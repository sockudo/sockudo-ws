#![cfg(feature = "tokio-runtime")]

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::protocol::Protocol;
use sockudo_ws::{Config, Error, Message, Role, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Default)]
struct RecordingIo {
    written: Vec<u8>,
    writes: usize,
    vectored_writes: usize,
    payload_address: usize,
    saw_payload: bool,
    max_write: Option<usize>,
    pending: bool,
    non_vectored: bool,
}

impl AsyncRead for RecordingIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for RecordingIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.writes += 1;
        self.saw_payload |= bytes.as_ptr() as usize == self.payload_address;
        let n = self.max_write.unwrap_or(bytes.len()).min(bytes.len());
        self.written.extend_from_slice(&bytes[..n]);
        self.pending = self.max_write.is_some();
        Poll::Ready(Ok(n))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        slices: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.vectored_writes += 1;
        let mut remaining = self.max_write.unwrap_or(usize::MAX);
        let mut n = 0;
        for slice in slices {
            self.saw_payload |= slice.as_ptr() as usize == self.payload_address;
            let take = remaining.min(slice.len());
            self.written.extend_from_slice(&slice[..take]);
            remaining -= take;
            n += take;
            if remaining == 0 {
                break;
            }
        }
        self.pending = self.max_write.is_some();
        Poll::Ready(Ok(n))
    }

    fn is_write_vectored(&self) -> bool {
        !self.non_vectored
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn large_server_payload_keeps_its_allocation_and_frame_order() {
    check_large_payload(None, false).await;
}

#[tokio::test]
async fn partial_segmented_writes_preserve_frame_order() {
    check_large_payload(Some(257), false).await;
}

#[tokio::test]
async fn non_vectored_transport_preserves_segment_order() {
    check_large_payload(Some(257), true).await;
}

async fn check_large_payload(max_write: Option<usize>, non_vectored: bool) {
    let payload = Bytes::from(vec![0xab; sockudo_ws::cork::ZERO_COPY_MIN + 17]);
    let io = RecordingIo {
        payload_address: payload.as_ptr() as usize,
        max_write,
        non_vectored,
        ..Default::default()
    };
    let mut ws = WebSocketStream::server(io, Config::default());
    let messages = [
        Message::text("first"),
        Message::Binary(payload),
        Message::text("last"),
    ];
    for message in &messages {
        ws.feed(message.clone()).await.unwrap();
    }
    ws.flush().await.unwrap();
    let io = ws.into_inner();
    assert!(
        io.saw_payload,
        "the transport must receive the original payload allocation"
    );
    let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
    let decoded = protocol
        .process(&mut BytesMut::from(io.written.as_slice()))
        .unwrap();
    assert_eq!(decoded.len(), messages.len());
    for (actual, expected) in decoded.iter().zip(&messages) {
        assert_eq!(actual.as_bytes(), expected.as_bytes());
        assert_eq!(actual.is_text(), expected.is_text());
    }
}

#[tokio::test]
async fn large_segment_counts_toward_backpressure_before_any_write() {
    let mut ws = WebSocketStream::server(
        RecordingIo::default(),
        Config::builder()
            .max_backpressure(sockudo_ws::cork::ZERO_COPY_MIN)
            .build(),
    );
    let result = ws
        .feed(Message::binary(vec![0; sockudo_ws::cork::ZERO_COPY_MIN]))
        .await;
    assert!(matches!(result, Err(Error::BufferFull)));
    assert!(matches!(ws.flush().await, Err(Error::ConnectionClosed)));
    assert!(ws.get_ref().written.is_empty());
}

#[tokio::test]
async fn send_flushes_even_when_inbound_messages_remain_queued() {
    let mut ws = WebSocketStream::from_raw_with_leftover(
        RecordingIo::default(),
        Role::Client,
        Config::default(),
        Some(Bytes::from_static(b"\x82\x01a\x82\x01b")),
    );
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"a");
    ws.send(Message::text("reply")).await.unwrap();
    let mut protocol = Protocol::new(Role::Server, 1024, 1024);
    let decoded = protocol
        .process(&mut BytesMut::from(ws.get_ref().written.as_slice()))
        .unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].as_text(), Some("reply"));
}

#[tokio::test]
async fn explicit_small_batch_uses_one_contiguous_write() {
    let mut ws = WebSocketStream::server(RecordingIo::default(), Config::default());
    ws.feed(Message::text("one")).await.unwrap();
    ws.feed(Message::text("two")).await.unwrap();
    assert!(ws.get_ref().written.is_empty());
    ws.flush().await.unwrap();
    assert_eq!(ws.get_ref().writes, 1);
    assert_eq!(ws.get_ref().vectored_writes, 0);
}

#[tokio::test]
async fn close_flushes_queued_segments_with_vectored_io() {
    let mut ws = WebSocketStream::server(RecordingIo::default(), Config::default());
    ws.feed(Message::binary(vec![0xab; sockudo_ws::cork::ZERO_COPY_MIN]))
        .await
        .unwrap();

    ws.close(1000, "done").await.unwrap();

    assert!(ws.get_ref().vectored_writes > 0);
    let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
    let messages = protocol
        .process(&mut BytesMut::from(ws.get_ref().written.as_slice()))
        .unwrap();
    assert!(matches!(
        &messages[..],
        [Message::Binary(_), Message::Close(_)]
    ));
}
