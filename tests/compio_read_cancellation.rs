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

    assert_eq!(
        compio::time::timeout(Duration::from_secs(4), ws.next())
            .await
            .expect("read stalled after its deadline")
            .unwrap()
            .unwrap()
            .as_bytes(),
        b"AAAAABBBBB"
    );
    assert_eq!(
        compio::time::timeout(Duration::from_secs(4), ws.next())
            .await
            .expect("read stalled after its deadline")
            .unwrap()
            .unwrap()
            .as_bytes(),
        b"xyz"
    );
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
    let mut encoder = DeflateEncoder::new(15, false, 6, 0);
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
        compio::time::timeout(Duration::from_secs(4), ws.next())
            .await
            .expect("read stalled after its deadline")
            .unwrap()
            .unwrap()
            .as_bytes(),
        payload.as_slice()
    );
    let _peer = send.await.unwrap();
}
