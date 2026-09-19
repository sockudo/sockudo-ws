#![cfg(feature = "permessage-deflate")]

use bytes::BytesMut;
use sockudo_ws::frame::encode_frame_with_rsv;
use sockudo_ws::{CompressedProtocol, DeflateConfig, Error, OpCode, Role};

#[test]
fn compressed_readers_reject_rsv1_on_control_and_continuation_frames() {
    for role in [Role::Client, Role::Server] {
        let mask = (role == Role::Server).then_some([1, 2, 3, 4]);
        for opcode in [
            OpCode::Continuation,
            OpCode::Close,
            OpCode::Ping,
            OpCode::Pong,
        ] {
            let make_protocol = || match role {
                Role::Client => CompressedProtocol::client(1024, 1024, DeflateConfig::default()),
                Role::Server => CompressedProtocol::server(1024, 1024, DeflateConfig::default()),
            };
            let mut wire = BytesMut::new();
            if opcode == OpCode::Continuation {
                encode_frame_with_rsv(&mut wire, OpCode::Text, b"x", false, mask, false);
            }
            encode_frame_with_rsv(&mut wire, opcode, b"", true, mask, true);
            let (mut reader, _) = make_protocol().split(1024, 1024);
            assert!(matches!(
                reader.process(&mut wire.clone()),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));
            assert!(matches!(
                make_protocol().process(&mut wire),
                Err(Error::Protocol("RSV1 on control or continuation frame"))
            ));
        }
    }
}
