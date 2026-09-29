#![cfg(feature = "tokio-runtime")]

use bytes::BytesMut;
use futures_util::{StreamExt, poll};
use rstest::rstest;
use sockudo_ws::frame::encode_frame;
use sockudo_ws::{Config, Error, Message, OpCode, Role, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[rstest]
#[case::text_small(OpCode::Text, 32)]
#[case::text_extended(OpCode::Text, 256)]
#[case::binary_small(OpCode::Binary, 32)]
#[case::binary_extended(OpCode::Binary, 256)]
#[tokio::test]
async fn data_delivery_preserves_payload_across_header_and_utf8_boundaries(
    #[case] opcode: OpCode,
    #[case] size: usize,
) {
    let mut payload = vec![b'a'; size - 3];
    payload.extend_from_slice("界".as_bytes());
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, opcode, &payload, true, None);
    for split_at in [0, 1, 2, 3, 4, wire.len() - 1, wire.len()] {
        for native_split in [false, true] {
            let (io, peer) = tokio::io::duplex(1024);
            let mut ws = WebSocketStream::from_raw_with_leftover(
                io,
                Role::Client,
                Config::builder().auto_ping(false).idle_timeout(0).build(),
                Some(wire.clone().freeze().slice(..split_at)),
            );
            if native_split {
                let (mut reader, _writer) = ws.split();
                receive_payload(reader.next(), peer, &wire[split_at..], &payload, opcode).await;
            } else {
                receive_payload(ws.next(), peer, &wire[split_at..], &payload, opcode).await;
            }
        }
    }
}

async fn receive_payload(
    next: impl Future<Output = Option<Result<Message, Error>>>,
    mut peer: tokio::io::DuplexStream,
    tail: &[u8],
    expected: &[u8],
    opcode: OpCode,
) {
    tokio::pin!(next);
    if !tail.is_empty() {
        assert!(poll!(&mut next).is_pending());
        peer.write_all(tail).await.unwrap();
    }
    let message = next.await.unwrap().unwrap();
    assert_eq!(message.is_text(), opcode == OpCode::Text);
    assert_eq!(message.as_bytes(), expected);
}

#[rstest]
#[case::text(OpCode::Text)]
#[case::binary(OpCode::Binary)]
#[tokio::test]
async fn unfinished_fragment_error_precedes_new_data_validation(#[case] opcode: OpCode) {
    let mut wire = BytesMut::new();
    encode_frame(&mut wire, OpCode::Text, b"a", false, None);
    encode_frame(&mut wire, opcode, &[0xff; 256], true, None);
    for native_split in [false, true] {
        let (io, _peer) = tokio::io::duplex(1024);
        let ws = WebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            Config::builder()
                .auto_ping(false)
                .idle_timeout(0)
                .max_message_size(128)
                .build(),
            Some(wire.clone().freeze()),
        );
        let result = if native_split {
            let (mut reader, _writer) = ws.split();
            reader.next().await
        } else {
            let mut reader = ws;
            reader.next().await
        };
        assert!(matches!(
            result,
            Some(Err(Error::Protocol("expected continuation frame")))
        ));
    }
}
