use bytes::Bytes;
use futures_util::StreamExt;
use sockudo_ws::deflate::{DeflateConfig, DeflateEncoder, MAX_WINDOW_BITS};
use sockudo_ws::stream::{CompressedSplitReader, CompressedWebSocketStream, SplitReader};
use sockudo_ws::{Config, Message, WebSocketStream};
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct Input {
    pub wire: Arc<Vec<u8>>,
    pub offset: usize,
    pub pending: bool,
    pub inject_pending: bool,
    pub max_read: usize,
    pub read_starts: Option<Arc<Mutex<Vec<usize>>>>,
}

impl AsyncRead for Input {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if let Some(starts) = &self.read_starts {
            // ReadBuf starts empty, so this points at the offered spare window.
            starts.lock().unwrap().push(buf.filled().as_ptr() as usize);
        }
        let end = (self.offset + buf.remaining().min(self.max_read)).min(self.wire.len());
        buf.put_slice(&self.wire[self.offset..end]);
        self.offset = if end == self.wire.len() { 0 } else { end };
        self.pending = self.inject_pending;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Input {
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

pub fn wire(payload: &[u8], compressed: bool, batch: usize) -> Arc<Vec<u8>> {
    let encoded = if compressed {
        DeflateEncoder::new(MAX_WINDOW_BITS, true, 1, 0)
            .compress(payload)
            .unwrap()
    } else {
        None
    };
    let bytes = encoded.as_deref().unwrap_or(payload);
    let mut frame = vec![if encoded.is_some() { 0xc2 } else { 0x82 }];
    match bytes.len() {
        0..=125 => frame.push(bytes.len() as u8),
        126..=65535 => {
            frame.push(126);
            frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(127);
            frame.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(bytes);
    Arc::new(frame.repeat(batch))
}

// Keep the four receiver types together without adding boxed ownership to this test.
#[allow(clippy::large_enum_variant)]
pub enum Reader {
    Plain(WebSocketStream<Input>),
    Split(SplitReader<Input>, sockudo_ws::stream::SplitWriter<Input>),
    Compressed(CompressedWebSocketStream<Input>),
    CompressedSplit(
        CompressedSplitReader<Input>,
        sockudo_ws::stream::CompressedSplitWriter<Input>,
    ),
}

impl Reader {
    pub fn new(input: Input, compressed: bool, split: bool) -> Self {
        let config = Config {
            auto_ping: false,
            idle_timeout: 0,
            ..Config::default()
        };
        if compressed {
            let deflate = DeflateConfig {
                server_no_context_takeover: true,
                client_no_context_takeover: true,
                ..DeflateConfig::default()
            };
            let stream = CompressedWebSocketStream::client(input, config, deflate);
            if split {
                let (r, w) = stream.split();
                Self::CompressedSplit(r, w)
            } else {
                Self::Compressed(stream)
            }
        } else {
            let stream = WebSocketStream::client(input, config);
            if split {
                let (r, w) = stream.split();
                Self::Split(r, w)
            } else {
                Self::Plain(stream)
            }
        }
    }
    pub async fn next(&mut self) -> Message {
        match self {
            Self::Plain(r) => r.next().await,
            Self::Split(r, _writer) => r.next().await,
            Self::Compressed(r) => r.next().await,
            Self::CompressedSplit(r, _writer) => r.next().await,
        }
        .unwrap()
        .unwrap()
    }
}

pub fn pattern(size: usize) -> Bytes {
    Bytes::from((0..size).map(|i| b'a' + (i % 23) as u8).collect::<Vec<_>>())
}
