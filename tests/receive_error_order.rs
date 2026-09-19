#![cfg(feature = "tokio-runtime")]

use futures_util::StreamExt;
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::AsyncWriteExt;

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

macro_rules! receive_cases {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;
            #[tokio::test]
            async fn accepted_message_precedes_parse_error() {
                let (io, mut peer) = tokio::io::duplex(128);
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x82\x01a\x83\x00").await.unwrap();
                assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");
                assert!(stream.next().await.unwrap().is_err());
                assert!(stream.next().await.is_none());
            }

            #[tokio::test]
            async fn peer_close_discards_later_parse_error() {
                let (io, mut peer) = tokio::io::duplex(128);
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x88\x02\x03\xe8\x83\x00").await.unwrap();
                assert!(stream.next().await.unwrap().unwrap().is_close());
                assert!(stream.next().await.is_none());
            }

            #[tokio::test]
            async fn ping_handling_does_not_reorder_accepted_messages() {
                let (io, mut peer) = tokio::io::duplex(128);
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x82\x01a\x89\x01p\x82\x01b\x83\x00")
                    .await
                    .unwrap();
                assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");
                assert!(stream.next().await.unwrap().unwrap().is_ping());
                assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"b");
                assert!(stream.next().await.unwrap().is_err());
                assert!(stream.next().await.is_none());
            }
        }
    };
}

receive_cases!(unified, |io| (WebSocketStream::client(io, config()), ()));
receive_cases!(split, |io| WebSocketStream::client(io, config()).split());
#[cfg(feature = "permessage-deflate")]
receive_cases!(compressed, |io| (
    sockudo_ws::CompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default()
    ),
    ()
));
#[cfg(feature = "permessage-deflate")]
receive_cases!(compressed_split, |io| {
    sockudo_ws::CompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    )
    .split()
});

#[tokio::test]
async fn split_preserves_an_error_after_a_message_from_handshake_leftover() {
    let (io, _peer) = tokio::io::duplex(128);
    let stream = WebSocketStream::from_raw_with_leftover(
        io,
        sockudo_ws::Role::Client,
        config(),
        Some(bytes::Bytes::from_static(b"\x82\x01a\x83\x00")),
    );
    let (mut stream, _writer) = stream.split();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}
