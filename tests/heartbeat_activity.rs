#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use futures_util::{StreamExt, poll};
use sockudo_ws::{Config, Error, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn fragments_postpone_automatic_ping_without_idle_timeout() {
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder().ping_interval(1).idle_timeout(0).build();
    let mut ws = WebSocketStream::client(io, config);

    tokio::time::advance(Duration::from_millis(600)).await;
    peer.write_all(b"\x01\x01a").await.unwrap();
    assert!(poll!(std::pin::pin!(ws.next())).is_pending());
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(poll!(std::pin::pin!(ws.next())).is_pending());
    let mut ping = [0; 14];
    assert!(poll!(std::pin::pin!(peer.read(&mut ping))).is_pending());

    tokio::time::advance(Duration::from_millis(401)).await;
    assert!(poll!(std::pin::pin!(ws.next())).is_pending());
    peer.read_exact(&mut ping).await.unwrap();
    // The client sends a masked Ping with an eight-byte heartbeat nonce.
    assert_eq!(&ping[..2], &[0x89, 0x88]);
}

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn incomplete_frame_bytes_do_not_refresh_idle_timeout() {
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let mut ws = WebSocketStream::client(io, config);

    tokio::time::advance(Duration::from_millis(600)).await;
    // The parser can consume this header, but its payload has not arrived.
    peer.write_all(b"\x01\x01").await.unwrap();
    assert!(poll!(std::pin::pin!(ws.next())).is_pending());
    tokio::time::advance(Duration::from_millis(401)).await;

    assert!(matches!(ws.next().await, Some(Err(Error::IdleTimeout))));
}

macro_rules! fragment_activity_case {
    ($name:ident, $socket:expr, $split:expr) => {
        #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
        #[tokio::test(start_paused = true)]
        async fn $name() {
            let (io, mut peer) = tokio::io::duplex(1024);
            let config = Config::builder().auto_ping(false).idle_timeout(1).build();
            let ws = ($socket)(io, config);
            let (mut reader, _writer) = ($split)(ws);

            // An empty continuation is still a complete valid frame. None of
            // these fragments produces a message before the final continuation.
            for frame in [b"\x01\x01a".as_slice(), b"\x00\x00", b"\x00\x01b"] {
                tokio::time::advance(Duration::from_millis(600)).await;
                peer.write_all(frame).await.unwrap();
                assert!(poll!(std::pin::pin!(reader.next())).is_pending());
                tokio::task::yield_now().await;
            }
            peer.write_all(b"\x80\x01c").await.unwrap();

            assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
            tokio::time::advance(Duration::from_millis(1001)).await;
            assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
        }
    };
}

fragment_activity_case!(
    unified_fragments_refresh_idle_until_the_message_finishes,
    WebSocketStream::client,
    |ws| (ws, ())
);

fragment_activity_case!(
    split_fragments_refresh_idle_until_the_message_finishes,
    WebSocketStream::client,
    |ws: WebSocketStream<_>| ws.split()
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_unified_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws| (ws, ())
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_split_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::CompressedWebSocketStream<_>| ws.split()
);
