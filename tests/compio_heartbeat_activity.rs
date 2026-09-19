#![cfg(feature = "compio-runtime")]

use std::time::Duration;

use bytes::BytesMut;
use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use sockudo_ws::frame::{OpCode, encode_frame_with_rsv};
use sockudo_ws::{CompioWebSocketStream, Config, Error};

#[compio::test]
async fn unified_read_preserves_partial_payload_across_ping_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let config = Config::builder().ping_interval(1).idle_timeout(0).build();
    let mut ws = CompioWebSocketStream::client(stream, config);

    peer.write_all(b"\x82\x0aAAAAA".to_vec()).await.0.unwrap();
    let send = compio::runtime::spawn(async move {
        compio::time::sleep(Duration::from_millis(1500)).await;
        peer.write_all(b"BBBBB\x82\x03xyz".to_vec())
            .await
            .0
            .unwrap();
        peer
    });

    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"AAAAABBBBB");
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"xyz");
    let _peer = send.await.unwrap();
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_unified_read_preserves_partial_payload_across_ping_deadline() {
    use sockudo_ws::deflate::{DeflateConfig, DeflateEncoder};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let config = Config::builder().ping_interval(1).idle_timeout(0).build();
    let mut ws = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        stream,
        config,
        DeflateConfig::default(),
    );
    let payload = vec![b'A'; 1024];
    let mut encoder = DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false, 6, 0);
    let compressed = encoder.compress(&payload).unwrap().unwrap();
    let mut frame = BytesMut::new();
    encode_frame_with_rsv(&mut frame, OpCode::Binary, &compressed, true, None, true);
    let split_at = frame.len() - 1;
    peer.write_all(frame[..split_at].to_vec()).await.0.unwrap();
    let send = compio::runtime::spawn(async move {
        compio::time::sleep(Duration::from_millis(1500)).await;
        peer.write_all(frame[split_at..].to_vec()).await.0.unwrap();
        peer
    });

    assert_eq!(
        ws.next().await.unwrap().unwrap().as_bytes(),
        payload.as_slice()
    );
    let _peer = send.await.unwrap();
}

macro_rules! fragment_activity_case {
    ($name:ident, $socket:expr, $split:expr) => {
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
                for frame in [
                    b"\x01\x01a".as_slice(),
                    b"\x00\x00",
                    b"\x00\x01b",
                    b"\x80\x01c",
                ] {
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
    |ws| (ws, ())
);

fragment_activity_case!(
    split_fragments_refresh_idle_until_the_message_finishes,
    CompioWebSocketStream::client,
    |ws: CompioWebSocketStream<_>| ws.split()
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_unified_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws| (ws, ())
);

#[cfg(feature = "permessage-deflate")]
fragment_activity_case!(
    compressed_split_fragments_refresh_idle_until_the_message_finishes,
    |io, config| sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config,
        sockudo_ws::deflate::DeflateConfig::default()
    ),
    |ws: sockudo_ws::compio::CompioCompressedWebSocketStream<_>| ws.split()
);
