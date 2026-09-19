#![cfg(feature = "tokio-runtime")]

use futures_util::StreamExt;
use sockudo_ws::{Config, Message, WebSocketStream};

macro_rules! completion_round_trip {
    ($name:ident, $server:expr, $client:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            let (io, peer) = tokio::io::duplex(128);
            let (_reader, mut writer) = ($server)(io).split();
            let mut receiver = ($client)(peer);
            let sender = tokio::spawn(async move {
                for sequence in 0..96_u8 {
                    let payload = (0..512)
                        .map(|i| (i as u8).wrapping_add(sequence))
                        .collect::<Vec<_>>();
                    writer.send(Message::binary(payload)).await.unwrap();
                    writer.flush().await.unwrap();
                }
                writer
            });

            for sequence in 0..96_u8 {
                let payload = receiver.next().await.unwrap().unwrap();
                let expected = (0..512)
                    .map(|i| (i as u8).wrapping_add(sequence))
                    .collect::<Vec<_>>();
                assert_eq!(payload.as_bytes(), expected);
            }

            let writer = sender.await.unwrap();
            assert!(!writer.is_closed());
        }
    };
}

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

completion_round_trip!(
    reused_completions_preserve_partial_writes_and_flushes,
    |io| WebSocketStream::server(io, config()),
    |io| WebSocketStream::client(io, config())
);

#[cfg(feature = "permessage-deflate")]
completion_round_trip!(
    reused_compressed_completions_preserve_dictionary_and_flushes,
    |io| sockudo_ws::CompressedWebSocketStream::server(
        io,
        config(),
        sockudo_ws::DeflateConfig::default()
    ),
    |io| sockudo_ws::CompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default()
    )
);
