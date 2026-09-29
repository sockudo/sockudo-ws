#![cfg(feature = "tokio-runtime")]

use bytes::BytesMut;
use futures_util::StreamExt;
use rstest::rstest;
use sockudo_ws::frame::encode_frame;
use sockudo_ws::{Config, Error, OpCode, Role, WebSocketStream};

#[rstest]
#[case::short(32)]
#[case::extended(256)]
#[tokio::test]
async fn complete_client_text_rejects_invalid_utf8(
    #[case] size: usize,
    #[values(false, true)] native_split: bool,
) {
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, &vec![0xff; size], true, None);
    let (io, _peer) = tokio::io::duplex(1024);
    let ws = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Client,
        Config::builder().auto_ping(false).idle_timeout(0).build(),
        Some(wire.freeze()),
    );

    let result = if native_split {
        let (mut reader, _writer) = ws.split();
        reader.next().await
    } else {
        let mut reader = ws;
        reader.next().await
    };

    assert!(matches!(result, Some(Err(Error::InvalidUtf8))));
}

#[rstest]
#[case::short(32)]
#[case::extended(256)]
#[tokio::test]
async fn complete_client_message_limit_precedes_text_validation(
    #[case] limit: usize,
    #[values(OpCode::Text, OpCode::Binary)] opcode: OpCode,
    #[values(false, true)] native_split: bool,
) {
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, opcode, &vec![0xff; limit + 1], true, None);
    let (io, _peer) = tokio::io::duplex(1024);
    let ws = WebSocketStream::from_raw_with_leftover(
        io,
        Role::Client,
        Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .max_message_size(limit)
            .build(),
        Some(wire.freeze()),
    );

    let result = if native_split {
        let (mut reader, _writer) = ws.split();
        reader.next().await
    } else {
        let mut reader = ws;
        reader.next().await
    };

    assert!(matches!(result, Some(Err(Error::MessageTooLarge))));
}
