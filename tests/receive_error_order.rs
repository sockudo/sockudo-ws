#![cfg(feature = "tokio-runtime")]

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

async fn read_masked_control_payload(peer: &mut tokio::io::DuplexStream, opcode: u8) -> Vec<u8> {
    let mut header = [0; 2];
    peer.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], 0x80 | opcode);
    assert_ne!(header[1] & 0x80, 0);
    let len = usize::from(header[1] & 0x7f);
    let mut mask = [0; 4];
    peer.read_exact(&mut mask).await.unwrap();
    let mut payload = vec![0; len];
    peer.read_exact(&mut payload).await.unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    payload
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
                assert!(!stream.is_closed());
                assert!(stream.next().await.unwrap().is_err());
                assert!(stream.is_closed());
                assert!(stream.next().await.is_none());
            }

            #[tokio::test]
            async fn peer_close_discards_later_parse_error() {
                let (io, mut peer) = tokio::io::duplex(128);
                let (mut stream, _guard) = ($make)(io);
                peer.write_all(b"\x88\x02\x03\xe8\x83\x00").await.unwrap();
                assert!(stream.next().await.unwrap().unwrap().is_close());
                assert!(stream.is_closed());
                assert!(stream.next().await.is_none());
                drop(stream);
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    read_masked_control_payload(&mut peer, 0x08),
                )
                .await
                .expect("the Close response must be written before the reader terminates");
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
                assert_eq!(
                    tokio::time::timeout(
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

macro_rules! unified_close_cases {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;

            #[tokio::test]
            async fn parse_error_after_peer_close_does_not_repeat_a_local_close() {
                let (io, mut peer) = tokio::io::duplex(128);
                let mut stream = ($make)(io);
                stream.close(1000, "").await.unwrap();
                assert_eq!(
                    read_masked_control_payload(&mut peer, 0x08).await,
                    b"\x03\xe8"
                );

                peer.write_all(b"\x88\x02\x03\xe8\x83\x00").await.unwrap();
                assert!(stream.next().await.unwrap().unwrap().is_close());
                assert!(stream.next().await.is_none());

                let mut extra = [0; 1];
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(1), peer.read(&mut extra))
                        .await
                        .expect("explicit Close must end the write half")
                        .unwrap(),
                    0,
                    "the peer Close must not trigger a second local Close"
                );
            }
        }
    };
}

unified_close_cases!(unified_local_close, |io| WebSocketStream::client(
    io,
    config()
));
#[cfg(feature = "permessage-deflate")]
unified_close_cases!(compressed_local_close, |io| {
    sockudo_ws::CompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    )
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

#[tokio::test]
async fn split_parse_error_stops_writes_before_messages_are_drained() {
    let (io, mut peer) = tokio::io::duplex(128);
    let (mut reader, mut writer) = WebSocketStream::client(io, config()).split();
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00").await.unwrap();

    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");

    assert!(!reader.is_closed());
    assert!(writer.is_closed());
    assert!(matches!(
        writer.send(sockudo_ws::Message::text("late")).await,
        Err(sockudo_ws::Error::ConnectionClosed)
    ));
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"b");
    assert!(reader.next().await.unwrap().is_err());
    assert!(reader.next().await.is_none());
}

#[tokio::test]
async fn unified_sink_close_after_parse_error_shuts_down_transport() {
    let (io, mut peer) = tokio::io::duplex(128);
    let mut stream = WebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00").await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    SinkExt::close(&mut stream).await.unwrap();

    let mut received = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_to_end(&mut received),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(received.is_empty());
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_sink_close_after_parse_error_shuts_down_transport() {
    let (io, mut peer) = tokio::io::duplex(128);
    let mut stream = sockudo_ws::CompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    );
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00").await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    SinkExt::close(&mut stream).await.unwrap();

    let mut received = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_to_end(&mut received),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(received.is_empty());
}

#[tokio::test]
async fn split_parse_error_does_not_cancel_an_in_progress_write() {
    let (io, mut peer) = tokio::io::duplex(64);
    let (mut reader, mut writer) = WebSocketStream::client(io, config()).split();
    let send = tokio::spawn(async move {
        writer
            .send(sockudo_ws::Message::binary(vec![0_u8; 1024]))
            .await
    });
    tokio::task::yield_now().await;

    peer.write_all(b"\x82\x01a\x83\x00").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");

    let mut frame = vec![0; 1032];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_exact(&mut frame),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(frame[0], 0x82);
    assert!(send.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn split_parse_error_bounds_an_in_progress_write() {
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let send = writer.send(sockudo_ws::Message::binary(vec![0_u8; 64]));
    tokio::pin!(send);
    assert!(futures_util::poll!(&mut send).is_pending());

    peer.write_all(b"\x82\x01a\x83\x00").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");
    assert!(reader.next().await.unwrap().is_err());
    tokio::task::yield_now().await;
    assert!(futures_util::poll!(&mut send).is_pending());

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_millis(1), &mut send)
            .await
            .expect("the read-error closing budget must terminate the blocked write"),
        Err(sockudo_ws::Error::ConnectionClosed)
    ));
}

#[tokio::test]
async fn split_parse_error_releases_transport_while_writer_lives() {
    let (io, mut peer) = tokio::io::duplex(128);
    let (mut reader, _writer) = WebSocketStream::client(io, config()).split();
    peer.write_all(b"\x82\x01a\x83\x00").await.unwrap();

    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");
    assert!(reader.next().await.unwrap().is_err());

    let mut received = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_to_end(&mut received),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(received.is_empty());
}

#[tokio::test]
async fn unified_parse_error_stops_writes_before_messages_are_drained() {
    use futures_util::SinkExt;
    let (io, mut peer) = tokio::io::duplex(128);
    let mut stream = WebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00").await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    let result = stream.send(sockudo_ws::Message::text("late")).await;

    assert!(matches!(result, Err(sockudo_ws::Error::ConnectionClosed)));
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"b");
    assert!(stream.next().await.unwrap().is_err());
}

#[tokio::test]
async fn splitting_after_parse_error_does_not_reopen_writes() {
    let (io, mut peer) = tokio::io::duplex(128);
    let mut stream = WebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x82\x01b\x83\x00").await.unwrap();
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

#[tokio::test]
async fn split_after_parse_error_still_replies_to_an_accepted_close() {
    use tokio::io::AsyncReadExt;
    let (io, mut peer) = tokio::io::duplex(128);
    let mut stream = WebSocketStream::client(io, config());
    peer.write_all(b"\x82\x01a\x88\x02\x03\xe8\x83\x00")
        .await
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_bytes(), b"a");

    let (mut reader, _writer) = stream.split();
    assert!(reader.next().await.unwrap().unwrap().is_close());
    let mut header = [0; 2];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        peer.read_exact(&mut header),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(header[0], 0x88);
}
