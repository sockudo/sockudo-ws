#![cfg(feature = "compio-runtime")]

use compio::io::{AsyncReadExt, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use sockudo_ws::{CompioWebSocketStream, Config};

async fn connection() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

async fn read_masked_control_payload(peer: &mut TcpStream, opcode: u8) -> Vec<u8> {
    let header = peer.read_exact(vec![0; 2]).await;
    header.0.unwrap();
    assert_eq!(header.1[0], 0x80 | opcode);
    assert_ne!(header.1[1] & 0x80, 0);
    let len = usize::from(header.1[1] & 0x7f);
    let mask = peer.read_exact(vec![0; 4]).await;
    mask.0.unwrap();
    let payload = peer.read_exact(vec![0; len]).await;
    payload.0.unwrap();
    payload
        .1
        .into_iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask.1[index % 4])
        .collect()
}

macro_rules! receive_cases {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;
            #[compio::test]
            async fn accepted_message_precedes_parse_error() {
                let (io, mut peer) = connection().await;
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x82\x01a\x83\x00".to_vec())
                    .await
                    .0
                    .unwrap();
                assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");
                assert!(!stream.is_closed());
                assert!(stream.next().await.unwrap().is_err());
                assert!(stream.is_closed());
                assert!(stream.next().await.is_none());
            }

            #[compio::test]
            async fn peer_close_discards_later_parse_error() {
                let (io, mut peer) = connection().await;
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x88\x02\x03\xe8\x83\x00".to_vec())
                    .await
                    .0
                    .unwrap();
                assert!(stream.next().await.unwrap().unwrap().is_close());
                assert!(stream.next().await.is_none());
            }

            #[compio::test]
            async fn ping_handling_does_not_reorder_accepted_messages() {
                let (io, mut peer) = connection().await;
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x82\x01a\x89\x01p\x82\x01b\x83\x00".to_vec())
                    .await
                    .0
                    .unwrap();
                assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");
                assert!(stream.next().await.unwrap().unwrap().is_ping());
                assert_eq!(
                    compio::time::timeout(
                        std::time::Duration::from_secs(1),
                        read_masked_control_payload(&mut peer, 0x0a),
                    )
                    .await
                    .unwrap(),
                    b"p"
                );
                assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"b");
                assert!(stream.next().await.unwrap().is_err());
                assert!(stream.next().await.is_none());
            }
        }
    };
}

receive_cases!(unified, |io| (
    CompioWebSocketStream::client(io, config()),
    ()
));
receive_cases!(split, |io| CompioWebSocketStream::client(io, config())
    .split());
#[cfg(feature = "permessage-deflate")]
receive_cases!(compressed, |io| (
    sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default()
    ),
    ()
));
#[cfg(feature = "permessage-deflate")]
receive_cases!(compressed_split, |io| {
    sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    )
    .split()
});

#[compio::test]
async fn split_parse_error_stops_writes_before_messages_are_drained() {
    let (io, mut peer) = connection().await;
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config()).split();
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00".to_vec())
        .await
        .0
        .unwrap();

    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");

    assert!(!reader.is_closed());
    assert!(writer.is_closed());
    assert!(matches!(
        writer.send(sockudo_ws::Message::text("late")).await,
        Err(sockudo_ws::Error::ConnectionClosed)
    ));
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"b");
    assert!(reader.next().await.unwrap().is_err());
}

#[compio::test]
async fn unified_parse_error_stops_writes_before_messages_are_drained() {
    let (io, mut peer) = connection().await;
    let mut stream = CompioWebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00".to_vec())
        .await
        .0
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    let result = stream.send(sockudo_ws::Message::text("late")).await;

    assert!(matches!(result, Err(sockudo_ws::Error::ConnectionClosed)));
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"b");
    assert!(stream.next().await.unwrap().is_err());
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_unified_parse_error_stops_writes_before_messages_are_drained() {
    let (io, mut peer) = connection().await;
    let mut stream = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    );
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00".to_vec())
        .await
        .0
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    let result = stream.send(sockudo_ws::Message::text("late")).await;

    assert!(matches!(result, Err(sockudo_ws::Error::ConnectionClosed)));
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"b");
    assert!(stream.next().await.unwrap().is_err());
}

#[compio::test]
async fn splitting_after_parse_error_does_not_reopen_writes() {
    let (io, mut peer) = connection().await;
    let mut stream = CompioWebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00".to_vec())
        .await
        .0
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    let (mut reader, mut writer) = stream.split();

    assert!(writer.is_closed());
    assert!(matches!(
        writer.send(sockudo_ws::Message::text("late")).await,
        Err(sockudo_ws::Error::ConnectionClosed)
    ));
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"b");
    assert!(reader.next().await.unwrap().is_err());
}

#[compio::test]
async fn split_after_parse_error_still_replies_to_an_accepted_close() {
    let (io, mut peer) = connection().await;
    let mut stream = CompioWebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x88\x02\x03\xe8\x83\x00".to_vec())
        .await
        .0
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    let (mut reader, _writer) = stream.split();
    assert!(reader.next().await.unwrap().unwrap().is_close());
    let read = compio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_exact(vec![0; 2]),
    )
    .await
    .unwrap();
    read.0.unwrap();

    assert_eq!(read.1[0], 0x88);
}
