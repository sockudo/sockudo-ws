#![cfg(feature = "tokio-runtime")]

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::{Sink, SinkExt, Stream, StreamExt, poll};
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

struct FailingWrite;

impl AsyncRead for FailingWrite {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for FailingWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("test write failure")))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn check_flush<S>(mut ws: S, mut peer: DuplexStream, send: bool)
where
    S: Sink<Message, Error = Error> + Stream<Item = Result<Message, Error>> + Unpin,
{
    peer.write_all(b"\x82\x01a\x82\x01b").await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"a");
    if send {
        ws.send(Message::text("reply")).await.unwrap();
    } else {
        ws.feed(Message::text("reply")).await.unwrap();
        ws.flush().await.unwrap();
    }
    let mut output = [0; 32];
    let read = peer.read(&mut output);
    tokio::pin!(read);
    let n = match poll!(&mut read) {
        Poll::Ready(Ok(n)) => n,
        other => panic!("successful flush must write the reply: {other:?}"),
    };
    assert_eq!(n, 11);
    assert_eq!(&output[..2], &[0x81, 0x85]);
    let decoded: Vec<_> = output[6..n]
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ output[2 + i % 4])
        .collect();
    assert_eq!(decoded, b"reply");
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"b");
}

#[tokio::test]
async fn flush_writes_with_unread_inbound_messages() {
    let (io, peer) = tokio::io::duplex(128);
    check_flush(WebSocketStream::client(io, Config::default()), peer, false).await;
}

#[tokio::test]
async fn send_writes_with_unread_inbound_messages() {
    let (io, peer) = tokio::io::duplex(128);
    check_flush(WebSocketStream::client(io, Config::default()), peer, true).await;
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_flush_writes_with_unread_inbound_messages() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, peer) = tokio::io::duplex(128);
    check_flush(
        CompressedWebSocketStream::client(io, Config::default(), DeflateConfig::default()),
        peer,
        false,
    )
    .await;
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_send_writes_with_unread_inbound_messages() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, peer) = tokio::io::duplex(128);
    check_flush(
        CompressedWebSocketStream::client(io, Config::default(), DeflateConfig::default()),
        peer,
        true,
    )
    .await;
}

#[tokio::test]
async fn explicit_flush_drains_a_buffered_feed() {
    let (io, mut peer) = tokio::io::duplex(128);
    let mut ws = WebSocketStream::client(io, Config::default());
    peer.write_all(b"\x82\x01a\x82\x01b").await.unwrap();
    ws.next().await.unwrap().unwrap();
    ws.feed(Message::text("reply")).await.unwrap();
    assert!(ws.write_buffer_len() > 0);
    ws.flush().await.unwrap();
    assert_eq!(ws.write_buffer_len(), 0);
    let mut output = [0; 32];
    let read = peer.read(&mut output);
    tokio::pin!(read);
    assert!(matches!(poll!(&mut read), Poll::Ready(Ok(n)) if n > 0));
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_explicit_flush_drains_a_buffered_feed() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, mut peer) = tokio::io::duplex(128);
    let mut ws = CompressedWebSocketStream::client(io, Config::default(), DeflateConfig::default());
    peer.write_all(b"\x82\x01a\x82\x01b").await.unwrap();
    ws.next().await.unwrap().unwrap();
    ws.feed(Message::text("reply")).await.unwrap();
    assert!(ws.write_buffer_len() > 0);
    ws.flush().await.unwrap();
    assert_eq!(ws.write_buffer_len(), 0);
    let mut output = [0; 32];
    let read = peer.read(&mut output);
    tokio::pin!(read);
    assert!(matches!(poll!(&mut read), Poll::Ready(Ok(n)) if n > 0));
}

macro_rules! high_water_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let (io, mut peer) = tokio::io::duplex(64);
            let mut ws = ($make)(io);
            // The unmasked header plus payload exactly reaches the default 64 KiB mark.
            ws.feed(Message::binary(vec![1; 65532])).await.unwrap();
            let mut next = std::pin::pin!(ws.feed(Message::binary(vec![2])));
            assert!(poll!(next.as_mut()).is_pending());
            let mut output = vec![0; 65536];
            let (sent, read) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(next, peer.read_exact(&mut output))
            })
            .await
            .unwrap();
            sent.unwrap();
            read.unwrap();
            assert_eq!(&output[..4], &[0x82, 126, 255, 252]);
            assert!(output[4..].iter().all(|byte| *byte == 1));
        }
    };
}
high_water_case!(feed_drains_at_the_high_water_mark, |io| {
    WebSocketStream::server(io, Config::default())
});
#[cfg(feature = "permessage-deflate")]
high_water_case!(compressed_feed_drains_at_the_high_water_mark, |io| {
    sockudo_ws::CompressedWebSocketStream::server(
        io,
        Config::default(),
        sockudo_ws::DeflateConfig {
            compression_threshold: usize::MAX,
            ..Default::default()
        },
    )
});

async fn check_high_water_write_error<S>(mut ws: S)
where
    S: Sink<Message, Error = Error> + Unpin,
{
    ws.feed(Message::binary(vec![1; 65532])).await.unwrap();
    let error = ws.feed(Message::binary(vec![2])).await.unwrap_err();
    assert!(matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::Other));
}

#[tokio::test]
async fn high_water_readiness_propagates_write_errors() {
    check_high_water_write_error(WebSocketStream::server(FailingWrite, Config::default())).await;
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_high_water_readiness_propagates_write_errors() {
    use sockudo_ws::{CompressedWebSocketStream, DeflateConfig};

    check_high_water_write_error(CompressedWebSocketStream::server(
        FailingWrite,
        Config::default(),
        DeflateConfig {
            compression_threshold: usize::MAX,
            ..Default::default()
        },
    ))
    .await;
}
