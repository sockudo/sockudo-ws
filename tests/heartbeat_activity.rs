#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use futures_util::{StreamExt, poll};
use sockudo_ws::{Config, Error, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

// Raw DEFLATE for "abc", with the permessage-deflate sync-flush tail removed.
fn fragments(compressed: bool) -> [&'static [u8]; 4] {
    if compressed {
        [
            b"\x41\x01\x4a",
            b"\x00\x00",
            b"\x00\x02\x4c\x4a",
            b"\x80\x02\x06\x00",
        ]
    } else {
        [b"\x01\x01a", b"\x00\x00", b"\x00\x01b", b"\x80\x01c"]
    }
}

macro_rules! fragment_activity_case {
    ($name:ident, $socket:expr, $split:expr, $compressed:expr) => {
        #[tokio::test(start_paused = true)]
        async fn $name() {
            let (io, mut peer) = tokio::io::duplex(1024);
            let config = Config::builder().auto_ping(false).idle_timeout(1).build();
            let ws = ($socket)(io, config);
            let (mut reader, _writer) = ($split)(ws);

            // An empty continuation is still a complete valid frame. None of
            // these fragments produces a message before the final continuation.
            for frame in &fragments($compressed)[..3] {
                tokio::time::advance(Duration::from_millis(600)).await;
                peer.write_all(frame).await.unwrap();
                assert!(poll!(std::pin::pin!(reader.next())).is_pending());
                tokio::task::yield_now().await;
            }
            peer.write_all(fragments($compressed)[3]).await.unwrap();

            assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
            tokio::time::advance(Duration::from_millis(1001)).await;
            assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
        }
    };
}

fragment_activity_case!(
    unified_fragments_refresh_idle_until_the_message_finishes,
    WebSocketStream::client,
    |ws| (ws, ()),
    false
);

fragment_activity_case!(
    split_fragments_refresh_idle_until_the_message_finishes,
    WebSocketStream::client,
    |ws: WebSocketStream<_>| ws.split(),
    false
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_unified_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws| (ws, ()),
    true
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_split_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::CompressedWebSocketStream<_>| ws.split(),
    true
);

macro_rules! partial_continuation_case {
    ($name:ident, $socket:expr, $split:expr, $compressed:expr) => {
        #[tokio::test(start_paused = true)]
        async fn $name() {
            let (io, mut peer) = tokio::io::duplex(1024);
            let config = Config::builder().auto_ping(false).idle_timeout(1).build();
            let ws = ($socket)(io, config);
            let (mut ws, _writer) = ($split)(ws);

            tokio::time::advance(Duration::from_millis(600)).await;
            peer.write_all(fragments($compressed)[0]).await.unwrap();
            assert!(poll!(std::pin::pin!(ws.next())).is_pending());
            tokio::time::advance(Duration::from_millis(600)).await;
            // A complete first fragment must not make a later partial frame activity.
            peer.write_all(b"\x00\x02b").await.unwrap();
            assert!(poll!(std::pin::pin!(ws.next())).is_pending());
            // Re-polling without input must not count as activity either.
            assert!(poll!(std::pin::pin!(ws.next())).is_pending());
            tokio::time::advance(Duration::from_millis(401)).await;
            tokio::task::yield_now().await;
            assert!(matches!(
                poll!(std::pin::pin!(ws.next())),
                std::task::Poll::Ready(Some(Err(Error::IdleTimeout)))
            ));
        }
    };
}

partial_continuation_case!(
    unified_partial_continuation_does_not_extend_fragment_activity,
    WebSocketStream::client,
    |ws| (ws, ()),
    false
);

partial_continuation_case!(
    split_partial_continuation_does_not_extend_fragment_activity,
    WebSocketStream::client,
    |ws: WebSocketStream<_>| ws.split(),
    false
);

#[cfg(feature = "permessage-deflate")]
partial_continuation_case!(
    compressed_unified_partial_continuation_does_not_extend_fragment_activity,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws| (ws, ()),
    true
);

#[cfg(feature = "permessage-deflate")]
partial_continuation_case!(
    compressed_split_partial_continuation_does_not_extend_fragment_activity,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::CompressedWebSocketStream<_>| ws.split(),
    true
);

macro_rules! pong_deadline_case {
    ($name:ident, $socket:expr, $split:expr, $compressed:expr) => {
        #[tokio::test(start_paused = true)]
        async fn $name() {
            let (io, mut peer) = tokio::io::duplex(1024);
            let config = Config::builder()
                .ping_interval(1)
                .pong_timeout(1)
                .idle_timeout(0)
                .build();
            let ws = ($socket)(io, config);
            let (mut reader, _writer) = ($split)(ws);
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(1001)).await;
            assert!(poll!(std::pin::pin!(reader.next())).is_pending());
            let mut ping = [0; 14];
            peer.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping[..2], &[0x89, 0x88]);
            tokio::time::advance(Duration::from_millis(600)).await;
            peer.write_all(fragments($compressed)[0]).await.unwrap();
            assert!(poll!(std::pin::pin!(reader.next())).is_pending());
            tokio::time::advance(Duration::from_millis(401)).await;
            assert!(matches!(
                reader.next().await,
                Some(Err(Error::HeartbeatTimeout))
            ));
        }
    };
}

pong_deadline_case!(
    unified_fragments_do_not_postpone_pong_deadline,
    WebSocketStream::client,
    |ws| (ws, ()),
    false
);

pong_deadline_case!(
    split_fragments_do_not_postpone_pong_deadline,
    WebSocketStream::client,
    |ws: WebSocketStream<_>| ws.split(),
    false
);

#[cfg(feature = "permessage-deflate")]
pong_deadline_case!(
    compressed_unified_fragments_do_not_postpone_pong_deadline,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws| (ws, ()),
    true
);

#[cfg(feature = "permessage-deflate")]
pong_deadline_case!(
    compressed_split_fragments_do_not_postpone_pong_deadline,
    |io, config| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::CompressedWebSocketStream<_>| ws.split(),
    true
);

#[cfg(feature = "permessage-deflate")]
#[tokio::test(start_paused = true)]
async fn compressed_split_leftover_fragment_refreshes_activity_when_accepted() {
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let ws = sockudo_ws::CompressedWebSocketStream::client_with_leftover(
        io,
        config,
        sockudo_ws::DeflateConfig::default(),
        Some(bytes::Bytes::from_static(fragments(true)[0])),
    );
    let (mut reader, _writer) = ws.split();
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;
    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
    for frame in &fragments(true)[1..] {
        peer.write_all(frame).await.unwrap();
    }
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
}

#[tokio::test(start_paused = true)]
async fn split_leftover_fragment_refreshes_activity_when_accepted() {
    let (io, mut peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let ws = WebSocketStream::from_raw_with_leftover(
        io,
        sockudo_ws::Role::Client,
        config,
        Some(bytes::Bytes::from_static(fragments(false)[0])),
    );
    let (mut reader, _writer) = ws.split();
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;
    assert!(poll!(std::pin::pin!(reader.next())).is_pending());
    for frame in &fragments(false)[1..] {
        peer.write_all(frame).await.unwrap();
    }
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
}
