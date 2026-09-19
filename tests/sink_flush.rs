#![cfg(feature = "tokio-runtime")]

use std::task::Poll;

use futures_util::{Sink, SinkExt, Stream, StreamExt, poll};
use sockudo_ws::{Config, Error, Message, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

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
    assert!(
        matches!(poll!(&mut read), Poll::Ready(Ok(n)) if n > 0),
        "successful flush must write even while inbound messages remain queued"
    );
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
async fn explicit_flush_drains_a_coalesced_send() {
    let (io, mut peer) = tokio::io::duplex(128);
    let mut ws = WebSocketStream::client(io, Config::default());
    peer.write_all(b"\x82\x01a\x82\x01b").await.unwrap();
    ws.next().await.unwrap().unwrap();
    ws.send_coalesced(Message::text("reply")).await.unwrap();
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
async fn compressed_explicit_flush_drains_a_coalesced_send() {
    use sockudo_ws::{CompressedWebSocketStream, deflate::DeflateConfig};
    let (io, mut peer) = tokio::io::duplex(128);
    let mut ws = CompressedWebSocketStream::client(io, Config::default(), DeflateConfig::default());
    peer.write_all(b"\x82\x01a\x82\x01b").await.unwrap();
    ws.next().await.unwrap().unwrap();
    ws.send_coalesced(Message::text("reply")).await.unwrap();
    assert!(ws.write_buffer_len() > 0);
    ws.flush().await.unwrap();
    assert_eq!(ws.write_buffer_len(), 0);
    let mut output = [0; 32];
    let read = peer.read(&mut output);
    tokio::pin!(read);
    assert!(matches!(poll!(&mut read), Poll::Ready(Ok(n)) if n > 0));
}
