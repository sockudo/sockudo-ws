#![cfg(feature = "tokio-runtime")]

use bytes::Bytes;
use futures_util::poll;
use sockudo_ws::{Config, Role, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn cancelled_next_retains_fragments_and_interleaved_controls() {
    let (io, mut peer) = tokio::io::duplex(128);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Client,
        config,
        Some(Bytes::from_static(b"\x01\x01a\x89\x01p\x00\x01b")),
    )
    .split();
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    {
        let next = reader.next();
        tokio::pin!(next);
        assert!(poll!(&mut next).is_pending());
    }

    peer.write_all(b"\x80\x01c\x82\x01d").await.unwrap();

    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"d");
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_precedes_an_unparsed_invalid_tail() {
    let (io, _peer) = tokio::io::duplex(128);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, _writer) = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Client,
        config,
        Some(Bytes::from_static(b"\x82\x01a\x83\x00")),
    )
    .split();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");
    tokio::task::yield_now().await;

    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;

    assert!(matches!(
        reader.next().await,
        Some(Err(sockudo_ws::Error::IdleTimeout))
    ));
    assert!(reader.next().await.is_none());
}
