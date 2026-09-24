#[cfg(any(feature = "tokio-runtime", feature = "compio-runtime"))]
struct SplitScenario {
    buffered: &'static [u8],
    remainder: &'static [u8],
    restored_is_text: bool,
    expected: &'static [u8],
}

#[cfg(any(feature = "tokio-runtime", feature = "compio-runtime"))]
const SPLIT_SCENARIOS: &[SplitScenario] = &[
    SplitScenario {
        buffered: b"\x82\x03one\x82",
        remainder: b"\x05hello\x82\x03two",
        restored_is_text: false,
        expected: b"hello",
    },
    SplitScenario {
        buffered: b"\x82\x03one\x82\x05he",
        remainder: b"llo\x82\x03two",
        restored_is_text: false,
        expected: b"hello",
    },
    SplitScenario {
        buffered: b"\x82\x03one\x01\x03hel",
        remainder: b"\x80\x02lo\x82\x03two",
        restored_is_text: true,
        expected: b"hello",
    },
    SplitScenario {
        buffered: b"\x82\x03one\x02\x03hel",
        remainder: b"\x80\x02lo\x82\x03two",
        restored_is_text: false,
        expected: b"hello",
    },
    SplitScenario {
        buffered: b"\x82\x03one\x81\x03\xe2",
        remainder: b"\x82\xac\x82\x03two",
        restored_is_text: true,
        expected: "€".as_bytes(),
    },
    SplitScenario {
        buffered: b"\x82\x03one\x01\x01\xe2\x00\x01\x82",
        remainder: b"\x80\x01\xac\x82\x03two",
        restored_is_text: true,
        expected: "€".as_bytes(),
    },
];

#[cfg(all(
    feature = "permessage-deflate",
    any(feature = "tokio-runtime", feature = "compio-runtime")
))]
fn compressed_fragment_scenario() -> (bytes::Bytes, bytes::Bytes, Vec<u8>, Vec<u8>) {
    use bytes::BytesMut;
    use sockudo_ws::OpCode;
    use sockudo_ws::deflate::DeflateEncoder;
    use sockudo_ws::frame::{encode_frame, encode_frame_with_rsv};

    let first = b"shared context payload shared context payload".repeat(16);
    let second = [first.as_slice(), b" and a second message"].concat();
    let mut encoder = DeflateEncoder::new(sockudo_ws::deflate::MAX_WINDOW_BITS, false, 6, 0);
    let first_compressed = encoder.compress(&first).unwrap().unwrap();
    let second_compressed = encoder.compress(&second).unwrap().unwrap();
    let fragment_at = second_compressed.len() / 2;
    assert!(fragment_at > 0);

    let mut buffered = BytesMut::new();
    encode_frame_with_rsv(
        &mut buffered,
        OpCode::Text,
        &first_compressed,
        true,
        None,
        true,
    );
    encode_frame_with_rsv(
        &mut buffered,
        OpCode::Text,
        &second_compressed[..fragment_at],
        false,
        None,
        true,
    );

    let mut remainder = BytesMut::new();
    encode_frame(
        &mut remainder,
        OpCode::Continuation,
        &second_compressed[fragment_at..],
        true,
        None,
    );
    encode_frame(&mut remainder, OpCode::Binary, b"two", true, None);

    (buffered.freeze(), remainder.freeze(), first, second)
}

#[cfg(feature = "tokio-runtime")]
use futures_util::StreamExt;

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn split_keeps_an_already_parsed_close_response() {
    use bytes::{Bytes, BytesMut};
    use sockudo_ws::frame::FrameParser;
    use sockudo_ws::{Config, Role, WebSocketStream};
    use tokio::io::AsyncReadExt;

    // Parsing "one" also accepts the following Close before split is called.
    let (io, mut peer) = tokio::io::duplex(64);
    let mut socket = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Client,
        Config::builder().auto_ping(false).idle_timeout(0).build(),
        Some(Bytes::from_static(b"\x82\x03one\x88\x05\x03\xe9bye")),
    );
    assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), b"one");
    let (mut reader, _writer) = socket.split();
    assert!(reader.next().await.unwrap().unwrap().is_close());
    let mut wire = BytesMut::with_capacity(64);
    tokio::time::timeout(std::time::Duration::from_secs(1), peer.read_buf(&mut wire))
        .await
        .unwrap()
        .unwrap();
    let response = FrameParser::new(64, true)
        .parse(&mut wire)
        .unwrap()
        .unwrap();
    assert_eq!(response.payload.as_ref(), b"\x03\xe9bye");
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn tokio_split_preserves_parser_and_fragment_state() {
    use bytes::Bytes;
    use sockudo_ws::{Config, Message, Role, WebSocketStream};
    use tokio::io::AsyncWriteExt;

    for scenario in SPLIT_SCENARIOS {
        let (io, mut peer) = tokio::io::duplex(64);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut socket = WebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            config,
            Some(Bytes::from_static(scenario.buffered)),
        );
        assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), b"one");

        let (mut reader, _writer) = socket.split();
        peer.write_all(scenario.remainder).await.unwrap();
        let restored = reader.next().await.unwrap().unwrap();
        assert_eq!(restored.as_bytes(), scenario.expected);
        assert_eq!(
            matches!(restored, Message::Text(_)),
            scenario.restored_is_text
        );
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"two");
    }
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn split_preserves_mask_offset_and_partial_utf8() {
    use bytes::BytesMut;
    use sockudo_ws::frame::encode_frame;
    use sockudo_ws::{Config, OpCode, Role, WebSocketStream};
    use tokio::io::AsyncWriteExt;

    let mask = Some([11, 22, 33, 44]);
    let mut prefix = BytesMut::new();
    encode_frame(&mut prefix, OpCode::Binary, b"one", true, mask);
    let mut text = BytesMut::new();
    encode_frame(&mut text, OpCode::Text, "€hello".as_bytes(), true, mask);
    // Six header/mask bytes, then only the first byte of the UTF-8 code point.
    prefix.extend_from_slice(&text.split_to(7));
    let (io, mut peer) = tokio::io::duplex(64);
    let mut socket = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Server,
        Config::builder().auto_ping(false).idle_timeout(0).build(),
        Some(prefix.freeze()),
    );
    assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), b"one");
    let (mut reader, _writer) = socket.split();
    peer.write_all(&text).await.unwrap();
    assert_eq!(
        reader.next().await.unwrap().unwrap().as_bytes(),
        "€hello".as_bytes()
    );
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_split_applies_message_limit_to_retained_fragments() {
    use bytes::BytesMut;
    use sockudo_ws::{CompressedProtocol, DeflateConfig, Error};

    for limit in [4, 5] {
        let mut protocol = CompressedProtocol::client(8192, 8192, DeflateConfig::default());
        let mut input = BytesMut::from(&b"\x02\x03abc\x80\x02d"[..]);
        assert!(protocol.process(&mut input).unwrap().is_empty());
        let (mut reader, _) = protocol.split(8192, limit);
        input.extend_from_slice(b"e");
        let result = reader.process(&mut input);
        if limit == 4 {
            assert!(matches!(result, Err(Error::MessageTooLarge)));
        } else {
            assert_eq!(result.unwrap()[0].as_bytes(), b"abcde");
        }
    }
}

#[cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]
#[tokio::test]
async fn tokio_compressed_split_preserves_parser_and_fragment_state() {
    use sockudo_ws::deflate::DeflateConfig;
    use sockudo_ws::{CompressedWebSocketStream, Config, Message};
    use tokio::io::AsyncWriteExt;

    for scenario in SPLIT_SCENARIOS {
        let (io, mut peer) = tokio::io::duplex(64);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut socket = CompressedWebSocketStream::client(io, config, DeflateConfig::default());
        peer.write_all(scenario.buffered).await.unwrap();
        assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), b"one");

        let (mut reader, _writer) = socket.split();
        peer.write_all(scenario.remainder).await.unwrap();
        let restored = reader.next().await.unwrap().unwrap();
        assert_eq!(restored.as_bytes(), scenario.expected);
        assert_eq!(
            matches!(restored, Message::Text(_)),
            scenario.restored_is_text
        );
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"two");
    }
}

#[cfg(all(feature = "tokio-runtime", feature = "permessage-deflate"))]
#[tokio::test]
async fn tokio_compressed_split_preserves_decoder_history_and_compressed_fragment_state() {
    use sockudo_ws::deflate::DeflateConfig;
    use sockudo_ws::{CompressedWebSocketStream, Config};
    use tokio::io::AsyncWriteExt;

    let (buffered, remainder, first, second) = compressed_fragment_scenario();
    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut socket = CompressedWebSocketStream::client(
        io,
        config,
        DeflateConfig {
            compression_threshold: 0,
            ..DeflateConfig::default()
        },
    );
    peer.write_all(&buffered).await.unwrap();
    assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), first);

    let (mut reader, _writer) = socket.split();
    peer.write_all(&remainder).await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), second);
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"two");
}

#[cfg(feature = "compio-runtime")]
#[compio::test]
async fn compio_split_preserves_parser_and_fragment_state() {
    use bytes::Bytes;
    use compio::io::AsyncWriteExt;
    use compio::net::{TcpListener, TcpStream};
    use sockudo_ws::{CompioWebSocketStream, Config, Message};

    for scenario in SPLIT_SCENARIOS {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut socket = CompioWebSocketStream::client_with_leftover(
            stream,
            config,
            Some(Bytes::from_static(scenario.buffered)),
        );
        assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), b"one");

        let (mut reader, _writer) = socket.split();
        peer.write_all(scenario.remainder.to_vec()).await.0.unwrap();
        let restored = reader.next().await.unwrap().unwrap();
        assert_eq!(restored.as_bytes(), scenario.expected);
        assert_eq!(
            matches!(restored, Message::Text(_)),
            scenario.restored_is_text
        );
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"two");
    }
}

#[cfg(all(feature = "compio-runtime", feature = "permessage-deflate"))]
#[compio::test]
async fn compio_compressed_split_preserves_parser_and_fragment_state() {
    use bytes::Bytes;
    use compio::io::AsyncWriteExt;
    use compio::net::{TcpListener, TcpStream};
    use sockudo_ws::compio::CompioCompressedWebSocketStream;
    use sockudo_ws::deflate::DeflateConfig;
    use sockudo_ws::{Config, Message};

    for scenario in SPLIT_SCENARIOS {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut socket = CompioCompressedWebSocketStream::client_with_leftover(
            stream,
            config,
            DeflateConfig::default(),
            Some(Bytes::from_static(scenario.buffered)),
        );
        assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), b"one");

        let (mut reader, _writer) = socket.split();
        peer.write_all(scenario.remainder.to_vec()).await.0.unwrap();
        let restored = reader.next().await.unwrap().unwrap();
        assert_eq!(restored.as_bytes(), scenario.expected);
        assert_eq!(
            matches!(restored, Message::Text(_)),
            scenario.restored_is_text
        );
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"two");
    }
}

#[cfg(all(feature = "compio-runtime", feature = "permessage-deflate"))]
#[compio::test]
async fn compio_compressed_split_preserves_decoder_history_and_compressed_fragment_state() {
    use compio::io::AsyncWriteExt;
    use compio::net::{TcpListener, TcpStream};
    use sockudo_ws::Config;
    use sockudo_ws::compio::CompioCompressedWebSocketStream;
    use sockudo_ws::deflate::DeflateConfig;

    let (buffered, remainder, first, second) = compressed_fragment_scenario();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut socket = CompioCompressedWebSocketStream::client_with_leftover(
        stream,
        config,
        DeflateConfig {
            compression_threshold: 0,
            ..DeflateConfig::default()
        },
        Some(buffered),
    );
    assert_eq!(socket.next().await.unwrap().unwrap().as_bytes(), first);

    let (mut reader, _writer) = socket.split();
    peer.write_all(remainder.to_vec()).await.0.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), second);
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"two");
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_protocol_split_preserves_partially_unmasked_payload() {
    use bytes::BytesMut;
    use sockudo_ws::{CompressedProtocol, DeflateConfig, Message};
    let config = DeflateConfig::default();
    let payload = "Hello, 世界! 🎉 ".repeat(100);
    let mut sender = CompressedProtocol::client(8192, 8192, config.clone());
    let mut wire = BytesMut::new();
    sender
        .encode_message(&Message::Text(payload.clone().into()), &mut wire)
        .unwrap();
    assert_ne!(wire[0] & 0x40, 0);
    let mut receiver = CompressedProtocol::server(8192, 8192, config);
    let mut buf = wire.split_to(wire.len() - 3);
    assert!(receiver.process(&mut buf).unwrap().is_empty());
    let (mut reader, _writer) = receiver.split(8192, 8192);
    buf.extend_from_slice(&wire);
    let messages = reader.process(&mut buf).unwrap();
    assert!(matches!(&messages[..], [Message::Text(t)] if t.as_ref() == payload.as_bytes()));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_protocol_split_can_lower_a_pending_frame_limit() {
    use bytes::BytesMut;
    use sockudo_ws::{CompressedProtocol, DeflateConfig, Error};
    let mut receiver = CompressedProtocol::client(8192, 8192, DeflateConfig::default());
    let mut buf = BytesMut::from(&b"\x82\x05a"[..]);
    assert!(receiver.process(&mut buf).unwrap().is_empty());
    let (mut reader, _) = receiver.split(4, 8192);
    buf.extend_from_slice(b"bcde");
    assert!(matches!(
        reader.process(&mut buf),
        Err(Error::FrameTooLarge)
    ));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_split_rejects_an_oversized_header_before_the_remaining_mask() {
    use bytes::BytesMut;
    use sockudo_ws::{CompressedProtocol, DeflateConfig, Error};
    let mut receiver = CompressedProtocol::server(8192, 8192, DeflateConfig::default());
    let mut buf = BytesMut::from(&b"\x82\x85\x00"[..]);
    assert!(receiver.process(&mut buf).unwrap().is_empty());
    let (mut reader, _) = receiver.split(4, 8192);
    // process() invokes the parser only for nonempty input; the mask is still incomplete.
    buf.extend_from_slice(b"\x00");
    assert!(matches!(
        reader.process(&mut buf),
        Err(Error::FrameTooLarge)
    ));
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_split_applies_limits_after_an_incomplete_extended_length() {
    use bytes::BytesMut;
    use sockudo_ws::frame::encode_frame;
    use sockudo_ws::{CompressedProtocol, DeflateConfig, Error, OpCode};
    for (size, cut) in [(126, 3), (65536, 6)] {
        let mut receiver = CompressedProtocol::client(131072, 131072, DeflateConfig::default());
        let mut wire = BytesMut::new();
        encode_frame(&mut wire, OpCode::Binary, &vec![b'x'; size], true, None);
        let mut buf = wire.split_to(cut);
        assert!(receiver.process(&mut buf).unwrap().is_empty());
        let (mut reader, _) = receiver.split(size - 1, 131072);
        buf.extend_from_slice(&wire);
        assert!(matches!(
            reader.process(&mut buf),
            Err(Error::FrameTooLarge)
        ));
    }
}

#[cfg(feature = "permessage-deflate")]
#[test]
fn compressed_split_accepts_a_pending_frame_at_the_new_limit() {
    use bytes::BytesMut;
    use sockudo_ws::frame::encode_frame;
    use sockudo_ws::{CompressedProtocol, DeflateConfig, OpCode};
    for cut in [3, 7] {
        let mut receiver = CompressedProtocol::server(8192, 8192, DeflateConfig::default());
        let mut wire = BytesMut::new();
        encode_frame(
            &mut wire,
            OpCode::Binary,
            b"abcde",
            true,
            Some([1, 2, 3, 4]),
        );
        let mut buf = wire.split_to(cut);
        assert!(receiver.process(&mut buf).unwrap().is_empty());
        let (mut reader, _) = receiver.split(5, 8192);
        buf.extend_from_slice(&wire);
        let messages = reader.process(&mut buf).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].as_bytes(), b"abcde");
    }
}
