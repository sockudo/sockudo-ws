#![cfg(feature = "tokio-runtime")]

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::AsyncReadExt;

macro_rules! queued_threshold_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            for threshold in [0, 10] {
                let (io, mut peer) = tokio::io::duplex(1);
                let config = Config::builder().max_backpressure(threshold).build();
                let mut ws = ($make)(io, config);
                // One 10-byte frame may reach or exceed the soft threshold.
                ws.feed(Message::Ping(bytes::Bytes::from_static(&[1; 8])))
                    .await
                    .unwrap();
                let next = ws.feed(Message::Ping(bytes::Bytes::from_static(&[2; 8])));
                let mut next = std::pin::pin!(next);
                assert!(futures_util::poll!(next.as_mut()).is_pending());
                let mut wire = [0; 10];
                let (sent, read) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(next, peer.read_exact(&mut wire))
                })
                .await
                .unwrap();
                sent.unwrap();
                read.unwrap();
                assert_eq!(wire, [0x89, 8, 1, 1, 1, 1, 1, 1, 1, 1]);
            }
        }
    };
}
queued_threshold_case!(
    unified_waits_for_queued_bytes_to_drain,
    WebSocketStream::server
);
#[cfg(feature = "permessage-deflate")]
queued_threshold_case!(compressed_waits_for_queued_bytes_to_drain, |io, config| {
    sockudo_ws::CompressedWebSocketStream::server(io, config, sockudo_ws::DeflateConfig::default())
});

macro_rules! large_message_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let (io, peer) = tokio::io::duplex(4096);
            let (mut writer, _guard) = ($make)(io);
            let mut reader = WebSocketStream::client(peer, Config::default());
            let payload = bytes::Bytes::from(vec![1; 2 * 1024 * 1024]);
            let send = async {
                writer.send(Message::Binary(payload.clone())).await.unwrap();
                writer.send(Message::binary(vec![2])).await.unwrap();
            };
            let receive = async {
                assert!(matches!(reader.next().await.unwrap().unwrap(), Message::Binary(data) if data == payload));
                assert!(matches!(reader.next().await.unwrap().unwrap(), Message::Binary(data) if data.as_ref() == [2]));
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(send, receive);
            }).await.unwrap();
        }
    };
}
large_message_case!(unified_preserves_default_large_message_sends, |io| (
    WebSocketStream::server(io, Config::default()),
    ()
));
large_message_case!(split_preserves_default_large_message_sends, |io| {
    let (reader, writer) = WebSocketStream::server(io, Config::default()).split();
    (writer, reader)
});
// Keep the encoded frame above the threshold when testing compressed streams.
#[cfg(feature = "permessage-deflate")]
large_message_case!(compressed_preserves_default_large_message_sends, |io| (
    sockudo_ws::CompressedWebSocketStream::server(
        io,
        Config::default(),
        sockudo_ws::DeflateConfig {
            compression_threshold: usize::MAX,
            ..Default::default()
        }
    ),
    ()
));
#[cfg(feature = "permessage-deflate")]
large_message_case!(
    compressed_split_preserves_default_large_message_sends,
    |io| {
        let (reader, writer) = sockudo_ws::CompressedWebSocketStream::server(
            io,
            Config::default(),
            sockudo_ws::DeflateConfig {
                compression_threshold: usize::MAX,
                ..Default::default()
            },
        )
        .split();
        (writer, reader)
    }
);
