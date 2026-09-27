#![cfg(feature = "compio-runtime")]

use std::time::Duration;

use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use sockudo_ws::{CompioWebSocketStream, Config, Error};

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
        #[compio::test]
        async fn $name() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let stream = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut peer, _) = listener.accept().await.unwrap();
            let config = Config::builder().auto_ping(false).idle_timeout(1).build();
            let ws = ($socket)(stream, config);
            let (mut reader, _writer) = ($split)(ws);
            let send = compio::runtime::spawn(async move {
                // Empty continuations also count as valid inbound frames.
                for frame in fragments($compressed) {
                    compio::time::sleep(Duration::from_millis(600)).await;
                    peer.write_all(frame.to_vec()).await.0.unwrap();
                }
                peer
            });

            assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"abc");
            let _peer = send.await.unwrap();
            assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
        }
    };
}

fragment_activity_case!(
    unified_fragments_refresh_idle_until_the_message_finishes,
    CompioWebSocketStream::client,
    |ws| (ws, ()),
    false
);

fragment_activity_case!(
    split_fragments_refresh_idle_until_the_message_finishes,
    CompioWebSocketStream::client,
    |ws: CompioWebSocketStream<_>| ws.split(),
    false
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_unified_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
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
    |io, config| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::compio::CompioCompressedWebSocketStream<_>| ws.split(),
    true
);

macro_rules! partial_continuation_case {
    ($name:ident, $socket:expr, $split:expr, $compressed:expr) => {
        #[compio::test]
        async fn $name() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let stream = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut peer, _) = listener.accept().await.unwrap();
            let config = Config::builder().auto_ping(false).idle_timeout(1).build();
            let ws = ($socket)(stream, config);
            let (mut reader, _writer) = ($split)(ws);
            let send = compio::runtime::spawn(async move {
                compio::time::sleep(Duration::from_millis(200)).await;
                peer.write_all(fragments($compressed)[0].to_vec())
                    .await
                    .0
                    .unwrap();
                compio::time::sleep(Duration::from_millis(600)).await;
                peer.write_all(b"\x00\x02b".to_vec()).await.0.unwrap();
                peer
            });
            // Correct activity expires around 1.2 s; counting the partial frame
            // would push it to 1.8 s. Keep the peer open throughout the check.
            let result = compio::time::timeout(Duration::from_millis(1550), reader.next()).await;
            let _peer = send.await.unwrap();
            assert!(matches!(result, Ok(Some(Err(Error::IdleTimeout)))));
        }
    };
}

partial_continuation_case!(
    unified_partial_continuation_does_not_extend_fragment_activity,
    CompioWebSocketStream::client,
    |ws| (ws, ()),
    false
);

partial_continuation_case!(
    split_partial_continuation_does_not_extend_fragment_activity,
    CompioWebSocketStream::client,
    |ws: CompioWebSocketStream<_>| ws.split(),
    false
);

#[cfg(feature = "permessage-deflate")]
partial_continuation_case!(
    compressed_unified_partial_continuation_does_not_extend_fragment_activity,
    |io, config| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
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
    |io, config| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::compio::CompioCompressedWebSocketStream<_>| ws.split(),
    true
);
